//! Host-side plumbing that gets host-sourced multicast into a running rack.
//!
//! The edge and transit routers are plain Linux boxes running FRR for unicast
//! BGP, with no multicast routing daemon, so a host route carries a group's
//! frames as far as `ce` and no further. In place of a daemon, a stock iproute2
//! `tc`/`mirred` ingress filter on the transit router mirrors each group from
//! the router's host-facing NIC onto its scrimlet-facing NICs, which is what
//! puts the frames on a switch.
//!
//! Two properties of that filter matter here. Every scrimlet is a mirror
//! target in a single filter per group, because external multicast ingresses
//! at whichever switch holds the group's external NAT entry, an election the
//! rack makes internally. The switches without the entry drop their copy, so
//! there is no duplicate replication. And the targets are chained actions
//! within one filter rather than one filter each: the first matching filter
//! ends flower classification, so a per-scrimlet filter would reach only the
//! first target unless every one of them carried an explicit `continue`,
//! whereas chained `mirred` actions all run under mirred's default `pipe`
//! control.
//!
//! The election is the only guard against a duplicate at this level. Viona's
//! ownership split between its classified and promiscuous receive callbacks
//! (viona_rx.c, stlouis#986) dedupes a different overlap, two local delivery
//! paths for one wire arrival, so a second copy forwarded by the other switch
//! would reach a guest as a second frame.
//!
//! The mirror only sees frames the router's NIC accepts, and a NIC accepts a
//! multicast group only after a join. Nothing on the router joins these groups
//! (FRR speaks no multicast protocol), so the flooded frames are dropped
//! before they reach the `tc` ingress hook. A static link-layer membership for
//! each group's Ethernet address (the RFC 1112 section 6.4 mapping) on the
//! host-facing NIC stands in for the join. Each membership `up` adds is
//! recorded under `/run` on the router, so `down` never removes one the
//! router already held, and the record dies with the router just as the
//! memberships do. This is voxel-only scaffolding for its mirror-based
//! emulated upstream. Customers do not add this Linux `ip maddress` entry to
//! a rack. Their upstream network must still deliver each group toward the
//! rack uplinks.
//!
//! The host route table lives in the global zone and is shared by Falcon
//! environments. Voxel records each environment's group and gateway under
//! `.falcon/`, so setup and teardown can leave routes owned by another
//! environment alone.
//!
//! This runs on the host unprivileged, escalating each mutating command
//! through `pfexec` (routes) or `ssh root@` (the router), the same path
//! `voxel commtest --setup-mcast` reuses before a run.
//!
//! See [RFC 1112 section 6.4](https://www.rfc-editor.org/rfc/rfc1112#section-6.4)
//! for the group-to-Ethernet mapping and
//! [RFC 7042 section 2.1.1](https://www.rfc-editor.org/rfc/rfc7042#section-2.1.1)
//! for the IANA OUI allotment behind it.

use anyhow::{Context, bail, ensure};
use itertools::Itertools;
use std::net::{IpAddr, Ipv4Addr};
use std::path::PathBuf;
use std::process::Command;
use voxel_config::VoxelConfig;

use crate::net::{
    ROUTE, RouteEntry, SshFailure, resolve_external_ip, route_entries,
    serial_bounded, ssh_capture, ssh_output, ssh_try_capture, zlogin,
};
use crate::network::SWADM;
use crate::topo::build_topo;
use crate::util::shell_quote;

/// The lowest `tc` filter priority voxel will claim. A group keeps whichever
/// pref its filter already holds, so repeated `up` runs replace in place, and
/// new groups take the next free one above this rather than displacing anything
/// else attached to the same ingress.
const PREF_BASE: u32 = 100;

/// The handle voxel gives every filter it installs. We make it explicit so
/// `replace` is idempotent (see `filter_cmd`) and, as part of `Filter::owned`,
/// so the filter a group resolves to is the same one `down`'s delete
/// addresses.
const HANDLE: u64 = 1;

/// The action voxel installs, as tc renders it in JSON: kind `mirred`, action
/// `mirror`, direction `egress`. This is shared between the install command
/// (`filter_cmd`) and detection (`TcAction::egress_mirror`) so the two cannot
/// drift apart.
const MIRRED: &str = "mirred";
const MIRROR: &str = "mirror";
const EGRESS: &str = "egress";
/// The netstat flag illumos uses for a host route.
const HOST_ROUTE_FLAG: char = 'H';

/// A host route installed for one Falcon environment.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
struct MulticastRoute {
    group: Ipv4Addr,
    gateway: String,
}

/// Host-side multicast state persisted per Falcon environment.
#[derive(Debug, serde::Deserialize, serde::Serialize)]
struct MulticastState {
    environment: String,
    routes: Vec<MulticastRoute>,
}

impl MulticastState {
    fn new(environment: &str) -> Self {
        Self { environment: environment.to_string(), routes: Vec::new() }
    }

    fn route(&self, group: &Ipv4Addr) -> Option<&MulticastRoute> {
        self.routes.iter().find(|route| route.group == *group)
    }

    fn groups(&self) -> impl Iterator<Item = Ipv4Addr> + '_ {
        self.routes.iter().map(|route| route.group)
    }

    fn set_route(&mut self, group: Ipv4Addr, gateway: String) {
        if let Some(route) = self.routes.iter_mut().find(|r| r.group == group) {
            route.gateway = gateway;
        } else {
            self.routes.push(MulticastRoute { group, gateway });
        }
    }

    fn remove_groups(&mut self, groups: &[Ipv4Addr]) {
        self.routes.retain(|route| !groups.contains(&route.group));
    }
}

/// The local state file is keyed by the Falcon environment name. Hex encoding
/// keeps arbitrary names out of the path while retaining one file per
/// environment.
fn multicast_state_path(name: &str) -> PathBuf {
    use std::fmt::Write as _;
    let mut key = String::with_capacity(name.len() * 2);
    for byte in name.bytes() {
        write!(&mut key, "{byte:02x}")
            .expect("writing to a String cannot fail");
    }
    PathBuf::from(".falcon").join(format!("multicast-{key}.json"))
}

fn read_multicast_state(name: &str) -> anyhow::Result<Option<MulticastState>> {
    let path = multicast_state_path(name);
    let body = match std::fs::read_to_string(&path) {
        Ok(body) => body,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(e)
                .with_context(|| format!("reading {}", path.display()));
        }
    };
    let state: MulticastState = serde_json::from_str(&body)
        .with_context(|| format!("parsing {}", path.display()))?;
    ensure!(
        state.environment == name,
        "{} belongs to Falcon environment '{}', not '{name}'",
        path.display(),
        state.environment,
    );
    Ok(Some(state))
}

fn write_multicast_state(
    name: &str,
    state: &MulticastState,
) -> anyhow::Result<()> {
    ensure!(
        state.environment == name,
        "multicast state environment '{}' does not match '{name}'",
        state.environment,
    );
    let path = multicast_state_path(name);
    if state.routes.is_empty() {
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("removing {}", path.display()));
            }
        }
        return Ok(());
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let tmp = path.with_extension("json.tmp");
    let body = serde_json::to_vec_pretty(state)
        .with_context(|| format!("serializing {}", path.display()))?;
    std::fs::write(&tmp, body)
        .with_context(|| format!("writing {}", tmp.display()))?;
    // A failed rename leaves the old record intact, which is what makes the
    // update atomic, but it also leaves the temporary behind to be mistaken
    // for a record later.
    if let Err(e) = std::fs::rename(&tmp, &path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).with_context(|| {
            format!("renaming {} to {}", tmp.display(), path.display())
        });
    }
    Ok(())
}

/// A group as passed to commtest: the bare address, or `GROUP@SRC,...` for a
/// source-filtered join. Only the address matters to the route and the mirror
/// (the source list is a property of the join), so strip any suffix.
///
/// TODO: IPv4 only, matching commtest's `validate_mcast`. Once that accepts
/// v6 groups, this file needs v6 throughout: the `33:33` membership mapping
/// (RFC 2464 section 7) in `group_mac` and `protocol ipv6` in `filter_cmd`.
///
/// The switch side already takes external IPv6 groups, and none of this
/// waits on new addressing: multicast frames carry the group MAC, so the
/// route through `ce` only pins the egress interface, which for v6 can be
/// `ce`'s link-local once the host plumbs addrconf on the segment (or the
/// sender selects the interface itself via `IPV6_MULTICAST_IF`).
fn group_addr(group: &str) -> anyhow::Result<Ipv4Addr> {
    let addr = group.split_once('@').map_or(group, |(addr, _)| addr);
    let addr: Ipv4Addr = addr
        .parse()
        .with_context(|| format!("multicast group '{group}' must be IPv4"))?;
    if !addr.is_multicast() {
        bail!("'{addr}' is not a multicast address (224.0.0.0/4)");
    }
    Ok(addr)
}

/// The distinct addresses `groups` include. This is deduplicated because a
/// repeated `--group` would otherwise install a second filter for the same
/// group at another pref, which `down` then cannot fully remove.
fn group_addrs(groups: &[String]) -> anyhow::Result<Vec<Ipv4Addr>> {
    groups
        .iter()
        .map(|g| group_addr(g))
        .process_results(|addrs| addrs.unique().collect())
}

/// The Ethernet address `group`'s frames carry on the wire: the group's
/// low-order 23 bits placed into `01:00:5e:00:00:00` (RFC 1112 section 6.4).
///
/// The prefix is IANA's OUI (`00-00-5E`), of whose 2^24 multicast identifiers
/// only the lower half is allotted to IPv4 (RFC 7042 section 2.1.1), so a
/// 28-bit group address maps onto 23 bits and 32 groups alias each Ethernet
/// address. Teardown, therefore, drops a membership only when no remaining
/// group still maps to it.
///
/// Note: Oxide's own OUI (`A8:40:25`) plays no part here. That prefix marks
/// Oxide-assigned unicast MACs, guest NICs included, while the multicast
/// mapping is protocol-defined and the same for every sender.
fn group_mac(addr: &Ipv4Addr) -> String {
    let [_, b, c, d] = addr.octets();
    format!("01:00:5e:{:02x}:{:02x}:{:02x}", b & 0x7f, c, d)
}

/// The distinct Ethernet addresses `addrs` map to, in order of first
/// occurrence.
fn group_macs(addrs: &[Ipv4Addr]) -> Vec<String> {
    addrs.iter().map(group_mac).unique().collect()
}

/// Split the MACs `down` is tearing down into those voxel may delete and
/// those it must leave alone. A membership goes only when no staying group
/// still maps to it and the router-side record says `up` created it.
fn deletable_members(
    addrs: &[Ipv4Addr],
    staying: &[Ipv4Addr],
    owned: &[String],
) -> (Vec<String>, Vec<String>) {
    let keep = group_macs(staying);
    group_macs(addrs)
        .into_iter()
        .filter(|m| !keep.contains(m))
        .partition(|m| owned.contains(m))
}

/// The router carrying the mirror: the first non-`ce` router, i.e. `cr1`.
///
/// One mirror suffices: every router sits on the host-facing segment and sees
/// the same flood, so mirroring from a second one would only duplicate frames
/// into the same switches.
fn mirror_router(cfg: &VoxelConfig) -> anyhow::Result<String> {
    cfg.topology
        .routers
        .iter()
        .find(|r| r.as_str() != "ce")
        .cloned()
        .context("topology.routers has no fabric router to mirror from")
}

/// A node's external address as voxel assigned it. This is `None` under DHCP
/// addressing, where addresses are leased and only discoverable from the
/// running node.
fn static_ip(cfg: &VoxelConfig, node: &str) -> Option<String> {
    cfg.external
        .static_addressing()
        .then(|| {
            cfg.static_external_ips()
                .into_iter()
                .find_map(|(n, ip)| (n == node).then_some(ip))
        })
        .flatten()
}

/// The mirror's shape, derived from config alone.
struct MirrorTarget {
    /// The router carrying the mirror (see `mirror_router`).
    router: String,
    /// The host-facing NIC the filter attaches to.
    iif: String,
    /// The scrimlet-facing NICs the filter mirrors onto.
    ifaces: Vec<String>,
}

impl MirrorTarget {
    /// The router, ingress NIC, and mirror devices the filter commands
    /// address. This is derived from config alone. The router's address is
    /// resolved separately (`node_addr`), which is the only step that can
    /// need the rack running.
    fn new(cfg: &VoxelConfig) -> anyhow::Result<Self> {
        let router = mirror_router(cfg)?;
        let ifaces = cfg.router_scrimlet_ifaces(&router);
        if ifaces.is_empty() {
            bail!("{router} has no scrimlet-facing NIC to mirror to");
        }
        let iif = cfg.router_ext_iface(&router);
        Ok(Self { router, iif, ifaces })
    }
}

/// A router's external address in either mode: isolated mode's static
/// assignment comes from config alone; otherwise, the node's DHCP lease is
/// read over the falcon console under `serial_bounded`'s two-stage deadline.
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

/// `ce`'s external address, the nexthop every group's host route points at. An
/// explicit `[topology] ce_external_ip` or isolated mode's static numbering
/// resolves without touching the guest. Otherwise `ce`'s lease is read from the
/// running node.
async fn ce_nexthop(cfg: &VoxelConfig, name: &str) -> anyhow::Result<String> {
    match crate::net::ce_static_ip(cfg) {
        Some(ip) => Ok(ip),
        None => node_addr(cfg, name, "ce").await,
    }
}

/// Run a command on the mirror router, or print it under `--dry-run`. Returns
/// its stdout and fails when ssh cannot reach the router or the command exits
/// non-zero.
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

/// Run a command on the mirror router, tolerating a non-zero exit. For the
/// deletes, which fail benignly when there is nothing to delete.
fn router_try(ip: &str, cmd: &str, dry_run: bool) {
    if dry_run {
        eprintln!("+ ssh root@{ip} {cmd}");
        return;
    }
    let _ = ssh_output(ip, cmd);
}

/// Run a host command under pfexec, or print it under `--dry-run`.
///
/// illumos `route` exits non-zero even on a successful add, so the status is
/// not checked here. `up` and `down` re-read the table instead.
fn host_route(args: &[&str], dry_run: bool) {
    if dry_run {
        eprintln!("+ pfexec route {}", args.join(" "));
        return;
    }
    let _ = Command::new("pfexec").arg(ROUTE).args(args).output();
}

/// The host-route gateways currently listed for `group`.
fn route_gateways(entries: &[RouteEntry], group: &Ipv4Addr) -> Vec<String> {
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

/// Whether a netstat route entry is a host route. Illumos prints route flags
/// as a compact set, such as `UGH`, rather than as a single enum value.
fn is_host_route(entry: &RouteEntry) -> bool {
    entry.flags.contains(HOST_ROUTE_FLAG)
}

/// Drop only the host routes whose gateways this environment owns.
fn purge_route(
    group: &Ipv4Addr,
    entries: &[RouteEntry],
    owned_gateways: &[String],
    dry_run: bool,
) {
    let group_text = group.to_string();
    for gateway in route_gateways(entries, group)
        .into_iter()
        .filter(|gateway| owned_gateways.contains(gateway))
    {
        host_route(&["delete", "-host", &group_text, &gateway], dry_run);
    }
}

/// The host routes belonging to `state` that remain for `addrs`.
fn remaining_host_routes(
    entries: &[RouteEntry],
    addrs: &[Ipv4Addr],
    state: &MulticastState,
) -> Vec<String> {
    addrs
        .iter()
        .filter_map(|addr| {
            let owned = state.route(addr)?;
            entries
                .iter()
                .find(|entry| {
                    entry.dest == addr.to_string()
                        && is_host_route(entry)
                        && entry.gateway == owned.gateway
                })
                .map(|entry| format!("host route {addr} -> {}", entry.gateway))
        })
        .collect()
}

/// Host routes for `addrs` that do not belong to `state`.
fn foreign_host_routes(
    entries: &[RouteEntry],
    addrs: &[Ipv4Addr],
    state: Option<&MulticastState>,
) -> Vec<String> {
    addrs
        .iter()
        .flat_map(|addr| {
            let owned = state
                .and_then(|state| state.route(addr))
                .map(|route| route.gateway.as_str());
            entries
                .iter()
                .filter(move |entry| {
                    entry.dest == addr.to_string()
                        && is_host_route(entry)
                        && Some(entry.gateway.as_str()) != owned
                })
                .map(move |entry| {
                    format!("host route {addr} -> {}", entry.gateway)
                })
        })
        .collect()
}

/// Name the host routes teardown is leaving in place, so a group that looks
/// torn down (but still resolves) is accounted for rather than silently skipped.
fn report_foreign_routes(
    addrs: &[Ipv4Addr],
    state: Option<&MulticastState>,
    name: &str,
) {
    for route in foreign_host_routes(&route_entries(), addrs, state) {
        eprintln!(
            "[voxel] multicast: leaving {route}; it is not owned by Falcon \
             environment '{name}'"
        );
    }
}

/// The groups recorded for a Falcon environment. This is what a groupless
/// `down` or `check` starts from rather than a scan of the host route table:
/// host routes are the one piece of the plumbing that outlives
/// `voxel destroy`, and the table is shared with every other environment, so
/// only this record says which of them are this environment's.
fn state_groups(state: Option<&MulticastState>) -> Vec<Ipv4Addr> {
    state.map(|state| state.groups().collect()).unwrap_or_default()
}

/// Remove the host routes recorded for a Falcon environment.
fn purge_state_routes(
    addrs: &[Ipv4Addr],
    state: Option<&MulticastState>,
    dry_run: bool,
) {
    let Some(state) = state else {
        return;
    };
    let entries = route_entries();
    for addr in addrs {
        let Some(route) = state.route(addr) else {
            continue;
        };
        purge_route(
            addr,
            &entries,
            std::slice::from_ref(&route.gateway),
            dry_run,
        );
    }
}

/// Whether the host route for `group` currently resolves to `nexthop`.
fn route_ok(group: &Ipv4Addr, nexthop: &str) -> bool {
    Command::new(ROUTE)
        .args(["-n", "get", &group.to_string()])
        .output()
        .map(|o| gateway_matches(&String::from_utf8_lossy(&o.stdout), nexthop))
        .unwrap_or(false)
}

/// Whether `route -n get` named `nexthop` as the gateway. This reads the
/// `gateway:` field rather than the whole output, so a nexthop that is a
/// prefix of another address on the segment cannot match by accident.
fn gateway_matches(out: &str, nexthop: &str) -> bool {
    out.lines()
        .filter_map(|l| l.trim().strip_prefix("gateway:"))
        .any(|gw| gw.trim() == nexthop)
}

/// An installed ingress filter, including the group it matches and the
/// devices its egress-mirror actions target.
struct Filter {
    pref: u32,
    handle: Option<u64>,
    kind: String,
    protocol: String,
    dst: Option<String>,
    mirrors: Vec<String>,
}

impl Filter {
    /// Whether this filter is one voxel installs: flower, protocol ip, at
    /// `HANDLE`, and in the pref range `free_pref` allocates from. `up` and
    /// `down` only replace or delete filters passing this, so a pre-existing
    /// filter that happens to match a group's address is left alone.
    fn owned(&self) -> bool {
        self.kind == "flower"
            && self.protocol == "ip"
            && self.handle == Some(HANDLE)
            && self.pref >= PREF_BASE
    }
}

/// One entry of `tc -json filter show`, restricted to the fields voxel reads.
/// Every field is optional because tc mixes real filters with per-pref summary
/// entries that carry no `options`. Only `pref` is required of an entry: one
/// missing `kind` or `protocol` still counts in `free_pref`'s accounting,
/// while `Filter::owned` rejects it.
#[derive(serde::Deserialize)]
struct TcFilter {
    pref: Option<u32>,
    kind: Option<String>,
    protocol: Option<String>,
    options: Option<TcOptions>,
}

impl TcFilter {
    /// Convert a `tc` entry into the subset of filter state voxel uses.
    fn into_filter(self) -> Option<Filter> {
        let Self { pref, kind, protocol, options } = self;
        let pref = pref?;
        let (handle, dst, mirrors) = options.map_or(
            (None, None, Vec::new()),
            |TcOptions { handle, keys, actions }| {
                (
                    handle,
                    keys.and_then(|keys| keys.dst_ip),
                    actions
                        .into_iter()
                        .filter(TcAction::egress_mirror)
                        .filter_map(|action| action.to_dev)
                        .collect(),
                )
            },
        );
        Some(Filter {
            pref,
            kind: kind.unwrap_or_default(),
            protocol: protocol.unwrap_or_default(),
            handle,
            dst,
            mirrors,
        })
    }
}

/// A handle other filter kinds may print as a string where flower prints a
/// number (compare the "ffff:" of `tc -json qdisc show`). Any non-numeric
/// form reads as `None`, keeping the entry rather than failing the read;
/// `Filter::owned` requires the numeric `HANDLE` anyway.
fn de_handle<'de, D>(d: D) -> Result<Option<u64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize;
    Ok(Option::<serde_json::Value>::deserialize(d)?.and_then(|v| v.as_u64()))
}

/// The `options` object of a `tc` filter entry.
#[derive(serde::Deserialize)]
struct TcOptions {
    #[serde(default, deserialize_with = "de_handle")]
    handle: Option<u64>,
    keys: Option<TcKeys>,
    #[serde(default)]
    actions: Vec<TcAction>,
}

/// The match keys of a `tc` filter entry. Flower (a pun on "flow-er") is
/// tc's classifier that matches on named packet-header fields rather than
/// raw byte offsets; `dst_ip` is the only key voxel's filters set. See
/// tc-flower(8).
#[derive(serde::Deserialize)]
struct TcKeys {
    dst_ip: Option<String>,
}

/// One action of a `tc` filter entry.
#[derive(serde::Deserialize)]
struct TcAction {
    kind: Option<String>,
    mirred_action: Option<String>,
    direction: Option<String>,
    to_dev: Option<String>,
}

impl TcAction {
    /// Whether this action mirrors to a device's egress. A redirect or an
    /// ingress action must not count as part of an installed mirror.
    fn egress_mirror(&self) -> bool {
        self.kind.as_deref() == Some(MIRRED)
            && self.mirred_action.as_deref() == Some(MIRROR)
            && self.direction.as_deref() == Some(EGRESS)
    }
}

/// Read the router's filters from `tc -json`, rather than scraping the
/// human-readable rendering. Anything but a JSON array is an error, so a
/// garbled read cannot pass for an empty ingress. An entry that does not fit
/// `TcFilter`'s shape (a foreign kind whose fields use other types) is
/// skipped on its own rather than failing the whole read.
///
/// The JSON keeps a filter's match under `options.keys` and each action's
/// kind, direction, and target as separate fields, so a redirect or an ingress
/// action cannot be mistaken for an egress mirror. Filters that match no
/// address (the per-pref summary entries tc emits) carry `dst: None`.
fn parse_filters(json: &str) -> anyhow::Result<Vec<Filter>> {
    let entries: Vec<serde_json::Value> =
        serde_json::from_str(json).context("parsing `tc -json filter show`")?;
    Ok(entries
        .into_iter()
        .filter_map(|e| serde_json::from_value::<TcFilter>(e).ok())
        .filter_map(TcFilter::into_filter)
        .collect())
}

/// The pref holding `group`'s filter, if voxel has one installed. This lets
/// `up` and `down` address a single group without disturbing filters
/// belonging to others, including a foreign filter matching the same address
/// (see `Filter::owned`).
fn group_pref(filters: &[Filter], group: &Ipv4Addr) -> Option<u32> {
    let group = group.to_string();
    filters
        .iter()
        .find(|f| f.owned() && f.dst.as_deref() == Some(group.as_str()))
        .map(|f| f.pref)
}

/// The multicast groups represented by filters voxel owns.
fn owned_group_addrs(
    filters: &[Filter],
) -> impl Iterator<Item = Ipv4Addr> + '_ {
    filters
        .iter()
        .filter(|filter| filter.owned())
        .filter_map(|filter| filter.dst.as_deref()?.parse().ok())
}

/// The lowest pref at or above `PREF_BASE` that is neither installed nor
/// already claimed by this run.
fn free_pref(filters: &[Filter], taken: &mut Vec<u32>) -> u32 {
    let pref = (PREF_BASE..)
        .find(|p| !filters.iter().any(|f| f.pref == *p) && !taken.contains(p))
        .expect("u32 range is not exhausted");
    taken.push(pref);
    pref
}

/// Whether `group`'s own filter mirrors to every device in `ifaces`. Scoped to
/// that filter, since devices belonging to a different group would otherwise
/// let a partial install pass.
fn mirror_installed(
    filters: &[Filter],
    group: &Ipv4Addr,
    ifaces: &[String],
) -> bool {
    let group = group.to_string();
    filters
        .iter()
        .filter(|f| f.owned() && f.dst.as_deref() == Some(group.as_str()))
        .any(|f| ifaces.iter().all(|dev| f.mirrors.contains(dev)))
}

/// One interface of `ip -json maddress show`, restricted to the fields voxel
/// reads.
#[derive(serde::Deserialize)]
struct MaddrIface {
    #[serde(default)]
    maddr: Vec<MaddrEntry>,
}

/// One membership entry.
///
/// Link-layer entries carry `link`, and the inet entries this reader skips
/// carry `family` and `address` instead.
#[derive(serde::Deserialize)]
struct MaddrEntry {
    link: Option<String>,
}

/// Read a NIC's link-layer memberships from `ip -json maddress show`, one
/// Ethernet address per entry. As with `parse_filters`, anything but a JSON
/// array is an error, so a garbled read cannot pass for a bare NIC.
fn parse_members(json: &str) -> anyhow::Result<Vec<String>> {
    let ifaces: Vec<MaddrIface> = serde_json::from_str(json)
        .context("parsing `ip -json maddress show`")?;
    Ok(ifaces
        .into_iter()
        .flat_map(|i| i.maddr)
        .filter_map(|m| m.link)
        .collect())
}

/// Marker error: the ssh transport could not reach the router.
///
/// `down` and the groupless `check` discovery treat only this failure as the
/// destroyed-rack case. A command that ran and failed propagates, so a broken
/// read cannot pass for a clean teardown or an empty rack.
#[derive(Debug)]
struct RouterUnreachable(String);

impl std::fmt::Display for RouterUnreachable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "router {} unreachable over ssh", self.0)
    }
}

impl std::error::Error for RouterUnreachable {}

/// Whether `err`'s chain contains [`RouterUnreachable`].
fn is_unreachable(err: &anyhow::Error) -> bool {
    err.chain().any(|c| c.downcast_ref::<RouterUnreachable>().is_some())
}

/// Wrap an [`SshFailure`] for `cmd` on `ip`, keeping the unreachable case
/// downcastable via [`is_unreachable`].
fn router_read_err(ip: &str, cmd: &str, e: SshFailure) -> anyhow::Error {
    match e {
        SshFailure::Unreachable => {
            anyhow::Error::new(RouterUnreachable(ip.to_string()))
        }
        SshFailure::Failed(text) => {
            anyhow::anyhow!("`{cmd}` on {ip} failed: {}", text.trim())
        }
    }
}

/// The router NIC's current link-layer memberships.
fn router_members(ip: &str, iif: &str) -> anyhow::Result<Vec<String>> {
    let cmd = format!("ip -json maddress show dev {iif}");
    match ssh_try_capture(ip, &cmd) {
        Ok(out) => {
            parse_members(&out).with_context(|| format!("`{cmd}` on {ip}"))
        }
        Err(e) => Err(router_read_err(ip, &cmd, e)),
    }
}

/// The router-side record of the memberships `up` added to `iif`, one
/// Ethernet address per line. Presence on the NIC alone cannot prove
/// ownership: the kernel maps 224.0.0.1 onto every interface, a join inside
/// the router adds its group's address, and 32 groups alias each one (see
/// `group_mac`).
///
/// This is kept under `/run` so the record dies with the router or
/// its reboot, exactly when the memberships themselves do, and a stale
/// record can never claim a membership on a later rack.
fn record_file(iif: &str) -> String {
    format!("/run/voxel-mcast-members.{iif}")
}

/// The memberships the record names as voxel's own. A missing record reads
/// as no memberships, which fails safe: `down` then leaves them alone. A
/// failed read is an error, so it cannot pass for an empty record and let a
/// teardown quietly skip the memberships.
fn recorded_members(ip: &str, iif: &str) -> anyhow::Result<Vec<String>> {
    let record = shell_quote(&record_file(iif));
    let cmd = format!("if [ -e {record} ]; then cat {record}; fi");
    let out =
        ssh_try_capture(ip, &cmd).map_err(|e| router_read_err(ip, &cmd, e))?;
    Ok(out.lines().map(str::to_string).collect())
}

/// Add a membership and record its ownership, rolling back the membership if
/// the record cannot be updated.
fn add_recorded_membership(
    ip: &str,
    iif: &str,
    mac: &str,
    dry_run: bool,
) -> anyhow::Result<()> {
    let mac_q = shell_quote(mac);
    let iif_q = shell_quote(iif);
    let record = shell_quote(&record_file(iif));
    let tmp = shell_quote(&format!("{}.tmp", record_file(iif)));
    let add = format!("ip maddress add {mac_q} dev {iif_q}");
    router_run(ip, &add, dry_run).map(drop)?;

    // Use a temporary file so a failed append cannot leave a partial record.
    // A missing record is the only non-match that should create a new one.
    let record_cmd = format!(
        "set -e; \
         if [ -e {record} ]; then \
             if grep -qxF {mac_q} {record}; then \
                 exit 0; \
             else \
                 status=$?; \
                 [ \"$status\" -eq 1 ] || exit \"$status\"; \
             fi; \
             cat {record} > {tmp}; \
         else \
             : > {tmp}; \
         fi; \
         printf '%s\\n' {mac_q} >> {tmp}; \
         mv {tmp} {record}"
    );
    if let Err(record_err) = router_run(ip, &record_cmd, dry_run).map(drop) {
        let rollback = format!("ip maddress del {mac_q} dev {iif_q}");
        return match router_run(ip, &rollback, false).map(drop) {
            Ok(()) => Err(anyhow::anyhow!(
                "recording membership {mac} on {iif} failed, \
                 membership rolled back: {record_err:#}"
            )),
            Err(rollback_err) => Err(anyhow::anyhow!(
                "recording membership {mac} on {iif} failed: {record_err:#}; \
                 rolling back the membership also failed: {rollback_err:#}"
            )),
        };
    }
    Ok(())
}

/// Remove a membership from the ownership record with an atomic replacement.
/// The record disappears with its last entry, keeping `down` free of leftover
/// state on the router.
fn remove_recorded_membership(
    ip: &str,
    iif: &str,
    mac: &str,
) -> anyhow::Result<()> {
    let mac_q = shell_quote(mac);
    let record_path = record_file(iif);
    let record = shell_quote(&record_path);
    let tmp = shell_quote(&format!("{record_path}.tmp"));
    let cmd = format!(
        "set -e; \
         if [ -e {record} ]; then \
             if grep -vxF {mac_q} {record} > {tmp}; then \
                 mv {tmp} {record}; \
             else \
                 status=$?; \
                 if [ \"$status\" -eq 1 ]; then \
                     rm -f {tmp} {record}; \
                 else \
                     rm -f {tmp}; \
                     exit \"$status\"; \
                 fi; \
             fi; \
         fi"
    );
    router_run(ip, &cmd, false)
        .map(drop)
        .with_context(|| format!("removing {mac} from {record_path}"))
}

/// The router's current ingress filters.
///
/// This reads even under `--dry-run`, which mutates nothing, so a preview
/// reports the prefs the real run would touch. A preview tolerates an
/// unreachable router.
fn show_filters(
    ip: &str,
    iif: &str,
    dry_run: bool,
) -> anyhow::Result<Vec<Filter>> {
    let cmd = format!("tc -json filter show dev {iif} ingress");
    match ssh_try_capture(ip, &cmd) {
        Ok(out) => {
            parse_filters(&out).with_context(|| format!("`{cmd}` on {ip}"))
        }
        Err(SshFailure::Unreachable) if dry_run => Ok(Vec::new()),
        Err(e) => Err(router_read_err(ip, &cmd, e)),
    }
}

/// The `tc` command installing a group's single filter: match the group's
/// address, then mirror to every scrimlet NIC through chained actions (see
/// the module doc for why the actions must chain).
fn filter_cmd(
    iif: &str,
    group: &Ipv4Addr,
    pref: u32,
    ifaces: &[String],
) -> String {
    // The handle has to be explicit for `replace` to be idempotent. Left at 0,
    // the kernel treats the request as a fresh insert, and flower rejects a
    // second filter carrying a key it already holds with EEXIST rather than
    // overwriting the first. Handles are scoped to a (pref, protocol), and this
    // installs one filter per pref, so HANDLE is always the one to replace.
    let mirrors: String = ifaces
        .iter()
        .map(|dev| format!(" action {MIRRED} {EGRESS} {MIRROR} dev {dev}"))
        .collect();
    format!(
        "tc filter replace dev {iif} ingress handle {HANDLE} pref {pref} protocol ip flower \
         dst_ip {group}{mirrors}"
    )
}

/// Point each group's host route at `ce`, install the mirror, and pin the
/// link-layer membership that lets the router accept each group. Safe to
/// re-run because this environment's routes are deleted before being re-added,
/// `tc filter replace` is idempotent and the membership add is guarded by a
/// lookup, with each add recorded on the router so `down` removes only
/// memberships voxel itself created. A route already owned by another Falcon
/// environment is rejected.
///
/// # Errors
///
/// Fails when a group is not an IPv4 multicast address, when `ce`'s or the
/// router's external address cannot be resolved, when the router is
/// unreachable, or when a host route does not take.
pub(crate) async fn up(
    cfg: &VoxelConfig,
    name: &str,
    groups: &[String],
    dry_run: bool,
) -> anyhow::Result<()> {
    let addrs = group_addrs(groups)?;
    let nexthop = ce_nexthop(cfg, name).await?;
    let MirrorTarget { router, iif, ifaces } = MirrorTarget::new(cfg)?;
    let router_ip = node_addr(cfg, name, &router).await?;
    let mut state = read_multicast_state(name)?
        .unwrap_or_else(|| MulticastState::new(name));

    eprintln!(
        "[voxel] multicast: {} group(s) via ce {nexthop}, mirroring {iif} -> {}",
        addrs.len(),
        ifaces.join(" ")
    );

    addrs.iter().try_for_each(|addr| {
        let group = addr.to_string();
        let entries = route_entries();
        let mut owned_gateways = vec![nexthop.clone()];
        if let Some(route) = state.route(addr)
            && !owned_gateways.contains(&route.gateway)
        {
            owned_gateways.push(route.gateway.clone());
        }
        let foreign: Vec<String> = route_gateways(&entries, addr)
            .into_iter()
            .filter(|gateway| !owned_gateways.contains(gateway))
            .collect();

        // The record, not the gateway, is what makes a route this
        // environment's. An unrecorded one may belong to another environment
        // or be a leftover whose record was removed, and the two are
        // indistinguishable from here, so neither is swept.
        ensure!(
            foreign.is_empty(),
            "host route {group} is not recorded for Falcon environment \
             '{name}' (gateway {})",
            foreign.join(", ")
        );
        // Record ownership before the route exists. A crash between the two
        // then leaves a record with no route, which `down` reads as nothing
        // to delete and the next `up` overwrites. The reverse ordering leaves
        // a route with no record, which nothing afterwards can prove is ours,
        // so a groupless `down` would have to leave it behind. The gateways
        // eligible for sweeping were captured above, so overwriting the entry
        // here does not lose the prior one.
        if !dry_run {
            state.set_route(*addr, nexthop.clone());
            write_multicast_state(name, &state).with_context(|| {
                format!("recording host route {group} for Falcon '{name}'")
            })?;
        }
        purge_route(addr, &entries, &owned_gateways, dry_run);
        host_route(&["add", "-host", &group, &nexthop], dry_run);
        ensure!(
            dry_run || route_ok(addr, &nexthop),
            "host route {group} -> {nexthop} did not take"
        );
        Ok(())
    })?;

    // Add the qdisc rather than recreating it, and take a pref per group rather
    // than the whole ingress: `up --group A` then `up --group B` has to leave A
    // working, and anything else attached here is not voxel's to delete.
    router_try(&router_ip, &format!("tc qdisc add dev {iif} clsact"), dry_run);
    let installed = show_filters(&router_ip, &iif, dry_run)?;
    addrs
        .iter()
        .scan(Vec::new(), |taken, addr| {
            Some((
                addr,
                group_pref(&installed, addr)
                    .unwrap_or_else(|| free_pref(&installed, taken)),
            ))
        })
        .try_for_each(|(addr, pref)| {
            router_run(
                &router_ip,
                &filter_cmd(&iif, addr, pref, &ifaces),
                dry_run,
            )
            .map(drop)
        })?;

    // The membership that lets the NIC accept each group's frames (see the
    // module doc). This is added only when absent, because `ip maddress add`
    // stacks reference counts and one `down` has to undo any number of `up`
    // runs.
    //
    // Each successful add lands in the router-side record, so `down` can tell
    // voxel's memberships from ones the router already held.
    let members = router_members(&router_ip, &iif)?;
    let owned = recorded_members(&router_ip, &iif)?;
    let (present, absent): (Vec<_>, Vec<_>) =
        group_macs(&addrs).into_iter().partition(|mac| members.contains(mac));
    for mac in present.iter().filter(|mac| !owned.contains(mac)) {
        eprintln!(
            "[voxel] multicast: membership {mac} on {iif} predates \
             voxel, leaving it to its owner"
        );
    }
    absent.iter().try_for_each(|mac| {
        add_recorded_membership(&router_ip, &iif, mac, dry_run)
    })?;
    if !dry_run {
        eprintln!("[voxel] multicast: up");
    }
    Ok(())
}

/// Remove each group's mirror filter, link-layer membership, and host routes.
/// Filters and memberships for other groups, memberships voxel did not
/// create, the `clsact` qdisc itself, the rack's own pool route, and the
/// external segment are all left alone.
///
/// With no groups given, this tears down everything this Falcon environment has
/// recorded or still owns on its router, so a groupless `down` undoes any
/// sequence of `up` runs without scanning another environment's host routes.
///
/// # Errors
///
/// Fails when a group is not an IPv4 multicast address, the router address
/// cannot be resolved, or anything survives the teardown. Deleting something
/// already absent is not an error, and neither is an unreachable router after
/// the host routes have been removed.
pub(crate) async fn down(
    cfg: &VoxelConfig,
    name: &str,
    groups: &[String],
    dry_run: bool,
) -> anyhow::Result<()> {
    let explicit = !groups.is_empty();
    let mut state = read_multicast_state(name)?;
    let mut addrs = if explicit {
        group_addrs(groups)?
    } else {
        state_groups(state.as_ref())
    };
    eprintln!(
        "[voxel] multicast: removing mirror filters, memberships, and host routes"
    );

    // Host routes go first. They are the one piece that outlives
    // `voxel destroy`, so they must be removed before attempting to inspect
    // the router. Only routes recorded for this environment are eligible.
    purge_state_routes(&addrs, state.as_ref(), dry_run);

    let MirrorTarget { router, iif, .. } = MirrorTarget::new(cfg)?;

    // Address discovery uses the serial console in LAN mode. Its failure does
    // not establish that the rack was destroyed, so propagate it instead of
    // reporting a successful teardown that may have left router state behind.
    let router_ip = match node_addr(cfg, name, &router).await {
        Ok(ip) => ip,
        Err(e) if dry_run => {
            eprintln!(
                "[voxel] multicast: cannot resolve {router} ({e:#}); \
                 skipping router preview"
            );
            return Ok(());
        }
        Err(e) => return Err(e),
    };
    let installed = match show_filters(&router_ip, &iif, dry_run) {
        Ok(filters) => filters,
        Err(e) if is_unreachable(&e) => {
            if !dry_run {
                report_foreign_routes(&addrs, state.as_ref(), name);
                if let Some(state) = state.as_ref() {
                    let remaining =
                        remaining_host_routes(&route_entries(), &addrs, state);
                    ensure!(
                        remaining.is_empty(),
                        "still plumbed after teardown:\n  {}",
                        remaining.join("\n  ")
                    );
                }
                if let Some(state) = state.as_mut() {
                    state.remove_groups(&addrs);
                    write_multicast_state(name, state)?;
                }
            }
            eprintln!(
                "[voxel] multicast: {e:#}: if the rack was destroyed, the \
                 mirror and memberships went with it"
            );
            return Ok(());
        }
        Err(e) => return Err(e),
    };

    // Without an explicit group list, the router's owned filters complete
    // the set: a group can keep a filter and membership after its host route
    // is gone, and the routes those groups may still hold go the same way as
    // the rest.
    if !explicit {
        let discovered: Vec<Ipv4Addr> = owned_group_addrs(&installed)
            .filter(|d| !addrs.contains(d))
            .collect();
        for d in discovered {
            addrs.push(d);
        }
    }
    report_foreign_routes(&addrs, state.as_ref(), name);

    // The install's full identifier tuple. A bare `pref` wildcards protocol
    // and handle and would take any other classifier sharing the priority
    // with it.
    addrs
        .iter()
        .filter_map(|addr| group_pref(&installed, addr))
        .for_each(|pref| {
            router_try(
                &router_ip,
                &format!(
                    "tc filter del dev {iif} ingress handle {HANDLE} pref {pref} protocol ip flower"
                ),
                dry_run,
            )
        });

    // Groups alias Ethernet addresses (see `group_mac`), so a membership goes
    // only when no group staying behind still maps to it, and only when the
    // router-side record says `up` created it. One that predates voxel (the
    // kernel's all-hosts mapping, or a join inside the router) stays with its
    // owner.
    let staying: Vec<Ipv4Addr> = installed
        .iter()
        .filter_map(|f| f.dst.as_deref()?.parse().ok())
        .filter(|d| !addrs.contains(d))
        .collect();
    let owned = recorded_members(&router_ip, &iif)?;
    let (dropped, foreign) = deletable_members(&addrs, &staying, &owned);
    if !foreign.is_empty() {
        let present = router_members(&router_ip, &iif)?;
        for mac in foreign.iter().filter(|m| present.contains(*m)) {
            eprintln!(
                "[voxel] multicast: membership {mac} on {iif} was not added \
                 by voxel, leaving it"
            );
        }
    }
    for mac in &dropped {
        router_try(
            &router_ip,
            &format!("ip maddress del {mac} dev {iif}"),
            dry_run,
        );
    }

    if dry_run {
        return Ok(());
    }

    // `router_try` swallows exit status, so a delete that failed for a real
    // reason is indistinguishable from one that had nothing to remove.
    // Re-read the state rather than reporting success blind.
    let left = show_filters(&router_ip, &iif, false)?;
    let members = router_members(&router_ip, &iif)?;
    let mut remaining: Vec<String> = addrs
        .iter()
        .flat_map(|addr| {
            group_pref(&left, addr)
                .map(|_| format!("mirror of {addr} on {iif}"))
                .into_iter()
        })
        .chain(
            dropped
                .iter()
                .filter(|mac| members.contains(*mac))
                .map(|mac| format!("membership {mac} on {iif}")),
        )
        .collect();
    if let Some(state) = state.as_ref() {
        remaining.extend(remaining_host_routes(
            &route_entries(),
            &addrs,
            state,
        ));
    }

    ensure!(
        remaining.is_empty(),
        "still plumbed after teardown:\n  {}",
        remaining.join("\n  ")
    );
    if let Some(state) = state.as_mut() {
        state.remove_groups(&addrs);
        write_multicast_state(name, state)?;
    }

    // With the deletes verified, retire their entries from the record. An
    // atomic replace rather than an in-place edit preserves the old record
    // when the update fails, and the error tells the caller that teardown is
    // incomplete.
    for mac in &dropped {
        remove_recorded_membership(&router_ip, &iif, mac)?;
    }

    let owned = recorded_members(&router_ip, &iif)?;
    let remaining: Vec<String> =
        dropped.iter().filter(|mac| owned.contains(*mac)).cloned().collect();
    ensure!(
        remaining.is_empty(),
        "ownership record still contains: {}",
        remaining.join(", ")
    );
    Ok(())
}

/// Every piece of plumbing for `groups`, each paired with whether it is in
/// place: the per-group host route, the router mirror, and the link-layer
/// membership.
///
/// One walk serves both consumers: [`check`] prints every item, and
/// [`missing_plumbing`] keeps only the absent ones.
pub(crate) async fn plumbing_status(
    cfg: &VoxelConfig,
    name: &str,
    groups: &[String],
) -> anyhow::Result<Vec<(String, bool)>> {
    let addrs = group_addrs(groups)?;
    let nexthop = ce_nexthop(cfg, name).await?;
    let MirrorTarget { router, iif, ifaces } = MirrorTarget::new(cfg)?;
    let router_ip = node_addr(cfg, name, &router).await?;

    let mut out: Vec<(String, bool)> = addrs
        .iter()
        .map(|addr| {
            (
                format!("host route {addr} -> {nexthop}"),
                route_ok(addr, &nexthop),
            )
        })
        .collect();
    let filters = match show_filters(&router_ip, &iif, false) {
        Ok(filters) => filters,
        // `check` reports rather than aborts, so a router that cannot be read
        // is one more missing item, with the reason attached.
        Err(e) => {
            out.push((format!("mirror on {iif}: {e:#}"), false));
            return Ok(out);
        }
    };
    out.extend(addrs.iter().map(|addr| {
        (
            format!("mirror of {addr} on {iif} -> {}", ifaces.join(" ")),
            mirror_installed(&filters, addr, &ifaces),
        )
    }));
    match router_members(&router_ip, &iif) {
        Ok(members) => out.extend(addrs.iter().map(|addr| {
            let mac = group_mac(addr);
            let present = members.contains(&mac);
            (format!("membership {mac} ({addr}) on {iif}"), present)
        })),
        // Report rather than abort here too.
        Err(e) => out.push((format!("memberships on {iif}: {e:#}"), false)),
    }
    Ok(out)
}

/// The pieces of plumbing missing for `groups`, as human-readable items.
/// Empty here means the path is complete.
///
/// This is the `commtest` preflight's view, so that a multicast run reports
/// the missing plumbing instead of a delivery failure.
pub(crate) async fn missing_plumbing(
    cfg: &VoxelConfig,
    name: &str,
    groups: &[String],
) -> anyhow::Result<Vec<String>> {
    Ok(plumbing_status(cfg, name, groups)
        .await?
        .into_iter()
        .filter_map(|(item, ok)| (!ok).then_some(item))
        .collect())
}

/// The groups this Falcon environment has plumbed, discovered from its
/// persisted host routes plus the selected router's owned mirror filters. An
/// unreachable router is the destroyed-rack case and contributes nothing.
/// Address-discovery failures and commands that ran and failed propagate, so a
/// broken read cannot hide router-only state.
/// Empty means no multicast is set up.
async fn plumbed_groups(
    cfg: &VoxelConfig,
    name: &str,
) -> anyhow::Result<Vec<String>> {
    let state = read_multicast_state(name)?;
    let mut addrs = state_groups(state.as_ref());
    let MirrorTarget { router, iif, .. } = MirrorTarget::new(cfg)?;
    let ip = node_addr(cfg, name, &router).await?;
    let router_groups: Vec<Ipv4Addr> = match show_filters(&ip, &iif, false) {
        Ok(filters) => owned_group_addrs(&filters).collect(),
        Err(e) if is_unreachable(&e) => {
            eprintln!("[voxel] multicast: {e:#}: counting host routes only");
            Vec::new()
        }
        Err(e) => return Err(e),
    };
    for d in router_groups {
        if !addrs.contains(&d) {
            addrs.push(d);
        }
    }
    Ok(addrs.iter().map(ToString::to_string).collect())
}

/// The external groups a switch has programmed, each with its NAT target,
/// parsed from `swadm multicast list` output. One row per group,
/// tab-aligned: GROUP IP, KIND, EXT GROUP ID, UL GROUP ID, TAG, DETAIL.
/// External rows carry the NAT target as `nat=<ip>` in DETAIL, `nat=-` when
/// none is set. The KIND column separates external rows from underlay ones
/// (an external group can be IPv6 too, so the address family cannot), and
/// the header never parses as an address, so no line count is assumed.
fn external_nat_targets(out: &str) -> Vec<(IpAddr, String)> {
    out.lines()
        .filter_map(|line| {
            let mut f = line.split_whitespace();
            let group = f.next()?.parse::<IpAddr>().ok()?;
            (f.next()? == "external").then_some(())?;
            let nat = f.find_map(|t| t.strip_prefix("nat="))?;
            Some((group, nat.to_string()))
        })
        .collect()
}

/// Print each group's control-plane mapping onto the underlay, read from
/// `swadm multicast list` in every switch zone. An external group's entry
/// names its NAT target, the admin-scoped underlay group the switch
/// replicates onto, which is the hop after the external path `check`
/// asserts.
///
/// This is read-only, so nothing here affects whether the check passes.
/// The entry appears once a multicast group is created against the rack
/// API (a commtest run does this), so a freshly plumbed group legitimately
/// has none, and a rack that is down or unreadable gets a note rather than
/// a failed check.
async fn print_underlay_mappings(
    cfg: &VoxelConfig,
    name: &str,
    groups: &[String],
) {
    let Ok(addrs) = group_addrs(groups) else {
        return;
    };
    // Isolated mode numbers every sled statically, so the falcon topology
    // (whose Runner construction logs at INFO) is only built when a
    // scrimlet's lease actually has to be read.
    let mut topo = None;
    for (idx, sled) in
        cfg.sleds().into_iter().filter(|s| s.scrimlet).enumerate()
    {
        let label = format!("switch{idx} ({})", sled.name);
        let ip = if let Some(ip) = static_ip(cfg, &sled.name) {
            ip
        } else {
            if topo.is_none() {
                let Ok(t) = build_topo(cfg, name) else { return };
                topo = Some(t);
            }
            let Some(t) = &topo else { return };
            let resolved = match t.node_ref(&sled.name) {
                Some(n) => {
                    serial_bounded(
                        &format!("reading {}'s address", sled.name),
                        resolve_external_ip(
                            cfg, &t.runner, &sled.name, n, false,
                        ),
                    )
                    .await
                }
                None => Err(anyhow::anyhow!("not in the topology")),
            };
            match resolved {
                Ok(ip) => ip,
                Err(_) => {
                    println!(
                        "underlay: {label} unresolvable, mapping unknown \
                         (is the rack up?)"
                    );
                    continue;
                }
            }
        };
        let Some(out) =
            ssh_capture(&ip, &zlogin(&format!("{SWADM} multicast list")))
        else {
            println!(
                "underlay: switch zone on {label} unreadable, mapping unknown"
            );
            continue;
        };
        let programmed = external_nat_targets(&out);
        if programmed.is_empty() {
            println!("underlay: no external groups programmed on {label}");
            continue;
        }
        for addr in &addrs {
            match programmed.iter().find(|(g, _)| *g == IpAddr::V4(*addr)) {
                Some((_, nat)) if nat != "-" => {
                    println!("underlay: {addr} -> {nat} on {label}")
                }
                Some(_) => {
                    println!("underlay: {addr} has no NAT target on {label}")
                }
                None => println!("underlay: {addr} not programmed on {label}"),
            }
        }
    }
}

/// Assert the whole external host path is live, printing one line per item.
///
/// With no groups given, this covers everything voxel has plumbed, the same
/// set a groupless `down` tears down, so any sequence of `up` runs is
/// asserted whole.
///
/// Only the external side is asserted here, and the trailing `underlay:`
/// lines show where the rack sends each group next (see
/// [`print_underlay_mappings`]). Proving underlay delivery past the switch
/// still needs a member in the group (a commtest run, or a joined probe
/// answering `ping`).
///
/// # Errors
///
/// Fails when any item is missing, so the CLI exit code reflects the result.
pub(crate) async fn check(
    cfg: &VoxelConfig,
    name: &str,
    groups: &[String],
) -> anyhow::Result<()> {
    let groups = if groups.is_empty() {
        let plumbed = plumbed_groups(cfg, name).await?;
        if plumbed.is_empty() {
            println!("check: nothing plumbed");
            return Ok(());
        }
        plumbed
    } else {
        groups.to_vec()
    };
    let status = plumbing_status(cfg, name, &groups).await?;
    let mut complete = true;
    for (item, ok) in &status {
        if *ok {
            println!("ok:      {item}");
        } else {
            println!("MISSING: {item}");
            complete = false;
        }
    }
    print_underlay_mappings(cfg, name, &groups).await;
    if complete {
        println!("check: PASS");
        Ok(())
    } else {
        bail!("check: FAIL")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn group_addr_strips_source_and_rejects_unicast() {
        assert_eq!(
            group_addr("232.100.0.1@192.168.1.199").unwrap(),
            Ipv4Addr::new(232, 100, 0, 1)
        );
        assert_eq!(
            group_addr("239.1.1.1").unwrap(),
            Ipv4Addr::new(239, 1, 1, 1)
        );
        assert!(group_addr("198.51.100.1").is_err());
        assert!(group_addr("ff05::1").is_err());
    }

    #[test]
    fn group_addrs_dedupes_repeats_in_first_occurrence_order() {
        let groups = vec![
            "239.1.1.2".to_string(),
            "239.1.1.1@192.168.1.199".to_string(),
            "239.1.1.2@10.0.0.1".to_string(),
            "239.1.1.1".to_string(),
        ];
        assert_eq!(
            group_addrs(&groups).unwrap(),
            vec![Ipv4Addr::new(239, 1, 1, 2), Ipv4Addr::new(239, 1, 1, 1)]
        );
    }

    #[test]
    fn external_nat_targets_keeps_external_rows_of_both_families() {
        // Shape of `swadm multicast list`: a header, external rows (v4 and
        // v6) whose DETAIL carries `nat=`, and underlay rows that share the
        // column layout but must not be counted as programmed groups.
        let out = "\
GROUP IP         KIND      EXT GROUP ID  UL GROUP ID  TAG    DETAIL
239.100.0.1      external  8             -            nexus  nat=ff04::e4:64:0:1 vlan=- src=any
ff05::100        external  9             -            nexus  nat=ff04::e4:0:0:100 vlan=2 src=any
239.100.0.9      external  10            -            nexus  nat=- vlan=- src=any
ff04::e4:64:0:1  underlay  8             11           nexus  rear0/0(underlay) rear1/0(underlay)
";
        assert_eq!(
            external_nat_targets(out),
            vec![
                ("239.100.0.1".parse().unwrap(), "ff04::e4:64:0:1".to_string()),
                ("ff05::100".parse().unwrap(), "ff04::e4:0:0:100".to_string()),
                ("239.100.0.9".parse().unwrap(), "-".to_string()),
            ]
        );
        assert!(external_nat_targets("GROUP IP  KIND\n").is_empty());
    }

    #[test]
    fn filter_chains_every_scrimlet_in_one_rule() {
        let ifaces = vec!["enp0s9".to_string(), "enp0s10".to_string()];
        let cmd =
            filter_cmd("enp0s11", &Ipv4Addr::new(239, 1, 1, 1), 100, &ifaces);
        assert_eq!(
            cmd,
            "tc filter replace dev enp0s11 ingress handle 1 pref 100 protocol ip flower \
             dst_ip 239.1.1.1 action mirred egress mirror dev enp0s9 \
             action mirred egress mirror dev enp0s10"
        );
    }

    /// `tc -json filter show dev enp0s11 ingress` with three voxel filters
    /// (239.1.1.1 fully mirrored, 239.1.1.2 mirrored to one NIC only, and
    /// 239.1.1.4 redirecting rather than mirroring) plus two foreign ones: a
    /// flower filter below `PREF_BASE` matching 239.1.1.1's address, and a
    /// matchall filter sitting inside the pref range.
    const FILTERS: &str = r#"[
      {"protocol":"ip","pref":10,"kind":"flower","chain":0,
       "options":{"handle":1,"keys":{"eth_type":"ipv4","dst_ip":"239.1.1.1"},
        "actions":[
          {"order":1,"kind":"mirred","mirred_action":"mirror","direction":"egress","to_dev":"enp0s9"},
          {"order":2,"kind":"mirred","mirred_action":"mirror","direction":"egress","to_dev":"enp0s10"}]}},
      {"protocol":"all","pref":102,"kind":"matchall","chain":0,
       "options":{"handle":1,"actions":[]}},
      {"protocol":"ip","pref":100,"kind":"flower","chain":0},
      {"protocol":"ip","pref":100,"kind":"flower","chain":0,
       "options":{"handle":1,"keys":{"eth_type":"ipv4","dst_ip":"239.1.1.1"},
        "actions":[
          {"order":1,"kind":"mirred","mirred_action":"mirror","direction":"egress","to_dev":"enp0s9"},
          {"order":2,"kind":"mirred","mirred_action":"mirror","direction":"egress","to_dev":"enp0s10"}]}},
      {"protocol":"ip","pref":101,"kind":"flower","chain":0,
       "options":{"handle":1,"keys":{"eth_type":"ipv4","dst_ip":"239.1.1.2"},
        "actions":[
          {"order":1,"kind":"mirred","mirred_action":"mirror","direction":"egress","to_dev":"enp0s9"}]}},
      {"protocol":"ip","pref":103,"kind":"flower","chain":0,
       "options":{"handle":1,"keys":{"eth_type":"ipv4","dst_ip":"239.1.1.4"},
        "actions":[
          {"order":1,"kind":"mirred","mirred_action":"redirect","direction":"egress","to_dev":"enp0s9"},
          {"order":2,"kind":"mirred","mirred_action":"mirror","direction":"ingress","to_dev":"enp0s10"}]}}
    ]"#;

    #[test]
    fn mirror_installed_is_scoped_to_the_group_filter() {
        let filters = parse_filters(FILTERS).unwrap();
        let ifaces = vec!["enp0s9".to_string(), "enp0s10".to_string()];
        assert!(mirror_installed(
            &filters,
            &Ipv4Addr::new(239, 1, 1, 1),
            &ifaces
        ));
        // enp0s10 is only in the first group's filter, so the second is
        // partial.
        assert!(!mirror_installed(
            &filters,
            &Ipv4Addr::new(239, 1, 1, 2),
            &ifaces
        ));
        assert!(!mirror_installed(
            &filters,
            &Ipv4Addr::new(239, 1, 1, 3),
            &ifaces
        ));
        // Neither a redirect nor an ingress mirror counts as an egress mirror.
        assert!(!mirror_installed(
            &filters,
            &Ipv4Addr::new(239, 1, 1, 4),
            &ifaces
        ));
    }

    #[test]
    fn prefs_are_reused_per_group_and_never_collide() {
        let filters = parse_filters(FILTERS).unwrap();
        assert_eq!(
            group_pref(&filters, &Ipv4Addr::new(239, 1, 1, 2)),
            Some(101)
        );
        assert_eq!(group_pref(&filters, &Ipv4Addr::new(239, 1, 1, 3)), None);
        // The foreign filter at pref 10 matches 239.1.1.1's address but sits
        // outside voxel's pref range, so the group resolves to its own filter.
        assert_eq!(
            group_pref(&filters, &Ipv4Addr::new(239, 1, 1, 1)),
            Some(100)
        );

        // A new group takes the lowest pref no installed filter holds, voxel's
        // or not, and two new groups in one run do not land on the same one.
        let mut taken = Vec::new();
        assert_eq!(free_pref(&filters, &mut taken), 104);
        assert_eq!(free_pref(&filters, &mut taken), 105);
    }

    #[test]
    fn parse_filters_keeps_foreign_entries_for_pref_accounting() {
        // A u32 filter with a string handle, an entry with only a pref, and
        // an entry whose pref is not a number: the first two must keep their
        // prefs out of `free_pref`'s reach, the last is skipped alone.
        let json = r#"[
          {"protocol":"all","pref":100,"kind":"u32",
           "options":{"handle":"800::800"}},
          {"pref":101},
          {"protocol":"ip","pref":"bogus","kind":"flower"}
        ]"#;
        let filters = parse_filters(json).unwrap();
        assert_eq!(
            filters.iter().map(|f| f.pref).collect::<Vec<_>>(),
            vec![100, 101]
        );
        assert!(filters.iter().all(|f| !f.owned()));
        let mut taken = Vec::new();
        assert_eq!(free_pref(&filters, &mut taken), 102);
    }

    /// Capture of `tc -json filter show dev enp0s11 ingress` from a live cr1
    /// (iproute2 on the router image) after
    /// `up --group 239.1.1.1 --group 239.2.2.2`. Unlike the hand-written
    /// `FILTERS`, this keeps everything real output carries: each filter's
    /// per-pref summary stub without `options`, `not_in_hw`, `control_action`,
    /// and the index/ref/bind action fields.
    const CAPTURED_FILTERS: &str = include_str!("testdata/tc-filter-show.json");

    #[test]
    fn parse_filters_handles_a_real_capture() {
        let filters = parse_filters(CAPTURED_FILTERS).unwrap();
        // The summary stubs come through as unowned entries alongside the
        // full ones, so each pref appears twice.
        assert_eq!(
            filters.iter().map(|f| f.pref).collect::<Vec<_>>(),
            [100, 100, 101, 101]
        );
        assert_eq!(filters.iter().filter(|f| f.owned()).count(), 2);

        let groups = [Ipv4Addr::new(239, 1, 1, 1), Ipv4Addr::new(239, 2, 2, 2)];
        let ifaces = vec!["enp0s9".to_string(), "enp0s10".to_string()];
        assert_eq!(group_pref(&filters, &groups[0]), Some(100));
        assert_eq!(group_pref(&filters, &groups[1]), Some(101));
        assert!(mirror_installed(&filters, &groups[0], &ifaces));
        assert!(mirror_installed(&filters, &groups[1], &ifaces));

        let mut taken = Vec::new();
        assert_eq!(free_pref(&filters, &mut taken), 102);
    }

    #[test]
    fn group_mac_maps_the_low_23_bits() {
        assert_eq!(
            group_mac(&Ipv4Addr::new(239, 1, 1, 1)),
            "01:00:5e:01:01:01"
        );
        assert_eq!(
            group_mac(&Ipv4Addr::new(239, 255, 255, 255)),
            "01:00:5e:7f:ff:ff"
        );
        // Groups differing only above bit 23 alias the same Ethernet address,
        // and the dedupe keeps one entry for both.
        let aliased = Ipv4Addr::new(224, 129, 1, 1);
        assert_eq!(group_mac(&aliased), "01:00:5e:01:01:01");
        assert_eq!(
            group_macs(&[Ipv4Addr::new(239, 1, 1, 1), aliased]),
            ["01:00:5e:01:01:01"]
        );
    }

    #[test]
    fn down_deletes_only_recorded_memberships() {
        let addrs =
            [Ipv4Addr::new(239, 1, 1, 1), Ipv4Addr::new(239, 128, 0, 1)];
        // Only the first group's membership is recorded as voxel's. The
        // second aliases the all-hosts address 01:00:5e:00:00:01, which the
        // kernel holds on every interface.
        let owned = ["01:00:5e:01:01:01".to_string()];
        let (dropped, foreign) = deletable_members(&addrs, &[], &owned);
        assert_eq!(dropped, ["01:00:5e:01:01:01"]);
        assert_eq!(foreign, ["01:00:5e:00:00:01"]);

        // A staying group aliasing the recorded membership keeps it.
        let staying = [Ipv4Addr::new(224, 129, 1, 1)];
        let (dropped, foreign) = deletable_members(&addrs, &staying, &owned);
        assert!(dropped.is_empty());
        assert_eq!(foreign, ["01:00:5e:00:00:01"]);
    }

    #[test]
    fn route_gateways_takes_host_routes_for_the_group_only() {
        let entry = |dest: &str, gw: &str, flags: &str| RouteEntry {
            dest: dest.into(),
            gateway: gw.into(),
            flags: flags.into(),
        };
        let entries = [
            // Header line, as route_entries passes it through.
            entry("Destination", "Gateway", "Flags"),
            // The interface route illumos holds for all of 224.0.0.0/4.
            entry("224.0.0.0", "172.30.199.2", "U"),
            // A unicast host route (the rack's external segment).
            entry("198.51.100.20", "172.30.199.14", "UGH"),
            entry("239.1.1.1", "172.30.199.14", "UGH"),
            // A stale duplicate from a prior launch.
            entry("239.1.1.1", "172.30.199.16", "UGH"),
            entry("239.2.2.2", "172.30.199.14", "UGH"),
        ];
        assert_eq!(
            route_gateways(&entries, &Ipv4Addr::new(239, 1, 1, 1)),
            ["172.30.199.14", "172.30.199.16"]
        );
        // The interface route covering the group is not a host route.
        assert!(
            route_gateways(&entries, &Ipv4Addr::new(224, 0, 0, 0)).is_empty()
        );
    }

    #[test]
    fn state_groups_come_from_the_record_alone() {
        let state = MulticastState {
            environment: "voxel".to_string(),
            routes: vec![
                MulticastRoute {
                    group: Ipv4Addr::new(239, 1, 1, 1),
                    gateway: "172.30.199.14".to_string(),
                },
                MulticastRoute {
                    group: Ipv4Addr::new(239, 2, 2, 2),
                    gateway: "172.30.199.14".to_string(),
                },
            ],
        };
        assert_eq!(
            state_groups(Some(&state)),
            [Ipv4Addr::new(239, 1, 1, 1), Ipv4Addr::new(239, 2, 2, 2)]
        );
        // No record means no groups, so a groupless `down` touches nothing.
        assert!(state_groups(None).is_empty());
    }

    #[test]
    fn route_scope_leaves_another_environment_alone() {
        let entry = |dest: &str, gw: &str| RouteEntry {
            dest: dest.into(),
            gateway: gw.into(),
            flags: "UGH".into(),
        };
        let entries = [
            entry("239.1.1.1", "172.30.199.14"),
            entry("239.1.1.1", "172.30.199.24"),
        ];
        let state = MulticastState {
            environment: "voxel".to_string(),
            routes: vec![MulticastRoute {
                group: Ipv4Addr::new(239, 1, 1, 1),
                gateway: "172.30.199.14".to_string(),
            }],
        };
        let group = Ipv4Addr::new(239, 1, 1, 1);

        assert_eq!(
            route_gateways(&entries, &group),
            ["172.30.199.14", "172.30.199.24"]
        );
        assert_eq!(
            remaining_host_routes(&entries, &[group], &state),
            ["host route 239.1.1.1 -> 172.30.199.14"]
        );
        assert_eq!(
            foreign_host_routes(&entries, &[group], Some(&state)),
            ["host route 239.1.1.1 -> 172.30.199.24"]
        );
    }

    #[test]
    fn parse_filters_rejects_non_json() {
        assert!(parse_filters("filter protocol ip pref 100 flower").is_err());
    }

    #[test]
    fn parse_members_keeps_link_layer_entries_only() {
        let json = r#"[
          {"ifindex":2,"ifname":"enp0s11","maddr":[
            {"link":"33:33:00:00:00:01"},
            {"family":"inet","address":"224.0.0.1"},
            {"link":"01:00:5e:01:01:01","users":2}]}]"#;
        assert_eq!(
            parse_members(json).unwrap(),
            ["33:33:00:00:00:01", "01:00:5e:01:01:01"]
        );
    }

    #[test]
    fn parse_members_rejects_non_json() {
        assert!(parse_members("1:\tlo\n\tinet  224.0.0.1\n").is_err());
    }

    #[test]
    fn gateway_matches_the_field_not_the_whole_output() {
        let out = "   route to: 239.1.1.1\ndestination: 239.1.1.1\n    gateway: 172.30.199.16\n  \
                   interface: voxel_ext0\n";
        assert!(gateway_matches(out, "172.30.199.16"));
        // A prefix of the real gateway must not pass.
        assert!(!gateway_matches(out, "172.30.199.1"));
        assert!(!gateway_matches(out, "239.1.1.1"));
    }

    #[test]
    fn mirror_target_derives_cr1_from_topology() {
        let cfg = VoxelConfig::from_toml("").unwrap();
        let MirrorTarget { router, iif, ifaces } =
            MirrorTarget::new(&cfg).unwrap();
        assert_eq!(router, "cr1");
        assert_eq!(iif, "enp0s11");
        assert_eq!(ifaces, ["enp0s9", "enp0s10"]);
    }
}
