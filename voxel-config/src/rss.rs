//! config-rss.toml generation.
//!
//! The types mirror omicron's RackInitializeRequest tree
//! (sled-agent/bootstrap-agent-lockstep-types, sled-agent/types
//! early_networking) only as far as serialization. Voxel writes this file and
//! never reads it, so the deserialize side, the validation newtypes and the
//! version-conversion machinery are omitted.
//!
//! Correctness is pinned by tests/rss.rs, which compares against golden files
//! captured from omicron's own types.
//!
//! toml emits a table's scalar fields in declaration order and hoists nested
//! tables to the end, so each struct is declared in omicron's field order.
//! Reordering a field changes the output. None fields are skipped by the toml
//! serializer, which is how omicron's optional knobs stay absent.

use std::collections::BTreeSet;
use std::net::{IpAddr, Ipv6Addr};

use serde::Serialize;

use crate::config::{RouterMode, UplinkPort, VoxelConfig};

/// A malformed address or prefix in the `voxel.toml`, named by its field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RssConfigError(String);

impl std::fmt::Display for RssConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for RssConfigError {}

type Result<T> = std::result::Result<T, RssConfigError>;

/// Parse `s`, tagging a failure with the config field it came from.
fn parse<T: std::str::FromStr>(s: &str, field: &str) -> Result<T> {
    s.parse::<T>()
        .map_err(|_| RssConfigError(format!("{field}: cannot parse {s:?}")))
}

// ---------------------------------------------------------------------------
// The wire types, in omicron's declaration order.
// ---------------------------------------------------------------------------

/// `bootstrap_agent_lockstep_types::RackInitializeRequest`.
#[derive(Debug, Serialize)]
struct RackInitializeRequest {
    trust_quorum_peers: Option<Vec<BaseboardId>>,
    bootstrap_discovery: BootstrapAddressDiscovery,
    ntp_servers: Vec<String>,
    dns_servers: Vec<IpAddr>,
    internal_services_ip_pool_ranges: Vec<IpRange>,
    external_dns_ips: Vec<IpAddr>,
    external_dns_zone_name: String,
    external_certificates: Vec<Certificate>,
    recovery_silo: RecoverySiloConfig,
    rack_network_config: RackNetworkConfig,
    allowed_source_ips: AllowedSourceIps,
    external_jumbo_frames_opt_in_enabled: bool,
}

/// `sled_hardware_types::BaseboardId`. Part number is declared first and so is
/// emitted first.
#[derive(Debug, Serialize)]
struct BaseboardId {
    part_number: String,
    serial_number: String,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum BootstrapAddressDiscovery {
    /// Only these addresses may join. `BTreeSet` so the emitted list is sorted,
    /// matching omicron's own field type.
    OnlyThese { addrs: BTreeSet<Ipv6Addr> },
}

/// `omicron_common::address::IpRange`. Omicron uses an untagged V4/V6 enum of
/// same-family ranges. Both variants serialize as `{first, last}`, so one
/// struct covers both. [`ip_range`] checks family agreement.
#[derive(Debug, Serialize)]
struct IpRange {
    first: IpAddr,
    last: IpAddr,
}

/// `Certificate`. Voxel emits none, but the field is required and must
/// serialize as an empty array.
#[derive(Debug, Serialize)]
struct Certificate {}

#[derive(Debug, Serialize)]
struct RecoverySiloConfig {
    silo_name: String,
    user_name: String,
    user_password_hash: String,
}

#[derive(Debug, Serialize)]
struct RackNetworkConfig {
    rack_subnet: oxnet::Ipv6Net,
    infra_ip_first: IpAddr,
    infra_ip_last: IpAddr,
    ports: Vec<PortConfig>,
    bgp: Vec<BgpConfig>,
    bfd: Vec<BfdPeerConfig>,
}

#[derive(Debug, Serialize)]
struct PortConfig {
    routes: Vec<RouteConfig>,
    addresses: Vec<UplinkAddressConfig>,
    switch: SwitchSlot,
    port: String,
    uplink_port_speed: LinkSpeed,
    uplink_port_fec: Option<LinkFec>,
    bgp_peers: Vec<BgpPeerConfig>,
    autoneg: bool,
    lldp: Option<LldpPortConfig>,
    tx_eq: Option<TxEqConfig>,
}

#[derive(Debug, Serialize)]
struct RouteConfig {
    destination: oxnet::IpNet,
    nexthop: IpAddr,
    vlan_id: Option<u16>,
    rib_priority: Option<u8>,
}

#[derive(Debug, Serialize)]
struct UplinkAddressConfig {
    address: UplinkAddress,
    vlan_id: Option<u16>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum UplinkAddress {
    /// Omicron renames this so config-rss reads `addrconf`, not `addr_conf`.
    #[serde(rename = "addrconf")]
    AddrConf,
    Static {
        ip_net: oxnet::IpNet,
    },
}

#[derive(Debug, Serialize)]
struct BgpPeerConfig {
    asn: u32,
    port: String,
    addr: RouterPeerType,
    hold_time: Option<u64>,
    idle_hold_time: Option<u64>,
    delay_open: Option<u64>,
    connect_retry: Option<u64>,
    keepalive: Option<u64>,
    remote_asn: Option<u32>,
    min_ttl: Option<u8>,
    md5_auth_key: Option<String>,
    multi_exit_discriminator: Option<u32>,
    communities: Vec<u32>,
    local_pref: Option<u32>,
    enforce_first_as: bool,
    allowed_import: ImportExportPolicy,
    allowed_export: ImportExportPolicy,
    vlan_id: Option<u16>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum RouterPeerType {
    /// `router_lifetime` is omicron's `RouterLifetimeConfig` newtype, which
    /// serializes as the bare integer.
    Unnumbered { router_lifetime: u16 },
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
enum ImportExportPolicy {
    NoFiltering,
}

#[derive(Debug, Serialize)]
struct BgpConfig {
    asn: u32,
    originate: Vec<oxnet::IpNet>,
    shaper: Option<String>,
    checker: Option<String>,
    /// Omicron's `MaxPathConfig` newtype over `u8`; its `Default` is 1.
    max_paths: u8,
}

#[derive(Debug, Serialize)]
struct BfdPeerConfig {
    local: Option<IpAddr>,
    remote: IpAddr,
    detection_threshold: u8,
    required_rx: u64,
    mode: BfdMode,
    switch: SwitchSlot,
}

#[derive(Debug, Serialize)]
struct LldpPortConfig {
    status: LldpAdminStatus,
    chassis_id: Option<String>,
    port_id: Option<String>,
    port_description: Option<String>,
    system_name: Option<String>,
    system_description: Option<String>,
    management_addrs: Option<Vec<IpAddr>>,
}

/// Declared for completeness so `PortConfig` mirrors omicron; voxel always
/// leaves `tx_eq` unset.
#[derive(Debug, Serialize)]
struct TxEqConfig {}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum SwitchSlot {
    Switch0,
    Switch1,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum LinkFec {
    None,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum LinkSpeed {
    Speed1G,
    Speed10G,
    Speed40G,
    Speed100G,
    Speed200G,
    Speed400G,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum LldpAdminStatus {
    Enabled,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum BfdMode {
    SingleHop,
}

#[derive(Debug, Serialize)]
#[serde(tag = "allow", rename_all = "snake_case")]
enum AllowedSourceIps {
    Any,
}

// ---------------------------------------------------------------------------
// voxel.toml -> the wire types.
// ---------------------------------------------------------------------------

fn switch_slot(s: &str) -> SwitchSlot {
    match s {
        "switch1" => SwitchSlot::Switch1,
        _ => SwitchSlot::Switch0,
    }
}

/// Omicron's `LinkSpeed` covers more rates than voxel's topologies use; unknown
/// values fall back to 40G, the a4x2 default.
fn link_speed(s: &str) -> LinkSpeed {
    match s {
        "1G" => LinkSpeed::Speed1G,
        "10G" => LinkSpeed::Speed10G,
        "100G" => LinkSpeed::Speed100G,
        "200G" => LinkSpeed::Speed200G,
        "400G" => LinkSpeed::Speed400G,
        _ => LinkSpeed::Speed40G,
    }
}

/// An `IpRange` from `first`/`last`. Omicron's `Ipv4Range::new` and
/// `Ipv6Range::new` require the same address family and `first <= last`.
fn ip_range(first: &str, last: &str) -> Result<IpRange> {
    let f: IpAddr = parse(first, "service_pool_first")?;
    let l: IpAddr = parse(last, "service_pool_last")?;
    match (f, l) {
        (IpAddr::V4(a), IpAddr::V4(b)) if a <= b => {}
        (IpAddr::V6(a), IpAddr::V6(b)) if a <= b => {}
        (IpAddr::V4(_), IpAddr::V4(_)) | (IpAddr::V6(_), IpAddr::V6(_)) => {
            return Err(RssConfigError(format!(
                "service pool: first {first} is after last {last}"
            )));
        }
        _ => {
            return Err(RssConfigError(
                "service pool: first/last must be the same address family".into(),
            ));
        }
    }
    Ok(IpRange { first: f, last: l })
}

/// A `PortConfig` with every optional knob unset. Voxel's ports differ only in
/// their routes, addresses and peers.
fn port_config(
    switch: &str,
    port: &str,
    speed: &str,
    description: String,
    routes: Vec<RouteConfig>,
    addresses: Vec<UplinkAddressConfig>,
    bgp_peers: Vec<BgpPeerConfig>,
) -> PortConfig {
    PortConfig {
        routes,
        addresses,
        switch: switch_slot(switch),
        port: port.to_string(),
        uplink_port_speed: link_speed(speed),
        uplink_port_fec: Some(LinkFec::None),
        bgp_peers,
        autoneg: false,
        lldp: Some(LldpPortConfig {
            status: LldpAdminStatus::Enabled,
            chassis_id: Some(switch.to_string()),
            port_id: None,
            port_description: Some(description),
            system_name: None,
            system_description: None,
            management_addrs: None,
        }),
        tx_eq: None,
    }
}

/// One uplink port toward a single fabric router (`UplinkPort` carries the
/// 2-way fanout: one port per router per switch). `Static` mode uses the port's
/// numbered /30 plus a default route; `Bgp` mode an unnumbered eBGP peer.
fn uplink_port(p: &UplinkPort, mode: RouterMode) -> Result<PortConfig> {
    let (routes, addresses, bgp_peers) = match mode {
        RouterMode::Bgp => (
            vec![],
            vec![UplinkAddressConfig {
                address: UplinkAddress::AddrConf,
                vlan_id: None,
            }],
            vec![BgpPeerConfig {
                asn: p.peer_asn,
                port: p.port.clone(),
                addr: RouterPeerType::Unnumbered {
                    router_lifetime: p.router_lifetime,
                },
                hold_time: None,
                idle_hold_time: None,
                delay_open: None,
                connect_retry: None,
                keepalive: None,
                remote_asn: None,
                min_ttl: None,
                md5_auth_key: None,
                multi_exit_discriminator: None,
                communities: vec![],
                local_pref: None,
                enforce_first_as: false,
                allowed_import: ImportExportPolicy::NoFiltering,
                allowed_export: ImportExportPolicy::NoFiltering,
                vlan_id: None,
            }],
        ),
        RouterMode::Static => (
            // Default route upstream via this router.
            vec![RouteConfig {
                destination: parse("0.0.0.0/0", "default route")?,
                nexthop: parse(&p.gateway, "uplink gateway")?,
                vlan_id: None,
                rib_priority: None,
            }],
            // Numbered /30 on the sidecar side.
            vec![UplinkAddressConfig {
                address: UplinkAddress::Static {
                    ip_net: parse(&p.sidecar_addr, "uplink /30")?,
                },
                vlan_id: None,
            }],
            // No BGP; BFD tracks the nexthop (rack_network_config.bfd).
            vec![],
        ),
    };
    Ok(port_config(
        &p.switch,
        &p.port,
        &p.port_speed,
        p.lldp.clone(),
        routes,
        addresses,
        bgp_peers,
    ))
}

/// A cross-rack sidecar interconnect port: link-local (`AddrConf`), no routes or
/// BGP - a "cluster port" for the multirack underlay. DDM (if it runs on the
/// port) carries the shared /48 across the mesh. 100G to match the
/// sidecar<->sidecar rear-port links.
fn interconnect_port(switch: &str, port: &str) -> PortConfig {
    port_config(
        switch,
        port,
        "100G",
        format!("interconnect-{port}"),
        vec![],
        // Link-local via addrconf so mg-ddm can peer cross-rack over the
        // interconnect. Nexus lot-validates this against the infra lot; the v6
        // block added to that lot lets it reserve in Static mode as it already
        // does in BGP mode.
        vec![UplinkAddressConfig {
            address: UplinkAddress::AddrConf,
            vlan_id: None,
        }],
        vec![],
    )
}

/// Build the typed request for a single rack (`rack`, 0-based) of a voxel
/// config. Multi-rack deployments call this once per rack: each rack is an
/// independent RSS domain, so the bootstrap set is filtered to that rack's
/// sleds and the customer/service network is offset by `Network::for_rack`.
fn request_from_config(cfg: &VoxelConfig, rack: usize) -> Result<RackInitializeRequest> {
    let n = cfg.network.for_rack(rack);

    let rss_sleds = || cfg.sleds().into_iter().filter(|s| s.rss && s.rack == rack);

    let bootstrap_addrs: BTreeSet<Ipv6Addr> = rss_sleds()
        .map(|s| parse(&s.bootstrap_addr(), "bootstrap addr"))
        .collect::<Result<_>>()?;

    let trust_quorum_peers: Vec<BaseboardId> = rss_sleds()
        .map(|s| BaseboardId {
            part_number: s.part_number.clone(),
            serial_number: s.serial_number.clone(),
        })
        .collect();

    let pool = ip_range(&n.service_pool_first, &n.service_pool_last)?;

    // 2-way fanout: one port per fabric router per switch (qsfp0, qsfp1, ...).
    let uplink_ports = cfg.uplink_ports(rack);
    let mut ports = Vec::new();
    for p in &uplink_ports {
        ports.push(uplink_port(p, n.router_mode)?);
    }
    // Cross-rack sidecar interconnect ports: link-local (AddrConf) cluster
    // ports, no routes/BGP, so DDM can carry the shared /48 underlay.
    for (sw, port) in cfg.interconnect_ports(rack) {
        ports.push(interconnect_port(&sw, &port));
    }
    if ports.is_empty() {
        // Omicron's UplinkPorts::new rejects an empty list. Fail here rather
        // than emitting a file sled-agent refuses.
        return Err(RssConfigError(
            "rack network config needs at least one uplink port".into(),
        ));
    }

    // Static-mode uplinks are numbered /30s; Nexus validates each switch-port
    // address against this lot at handoff. Derived from transit_prefix to cover
    // the sidecar /30s. Bgp uplinks are unnumbered, so the lot is unused.
    let (infra_first, infra_last): (IpAddr, IpAddr) = match n.router_mode {
        RouterMode::Static => {
            let (f, l) = n.infra_ip_range(uplink_ports.len()).ok_or_else(|| {
                RssConfigError(format!(
                    "infra_ip_range: cannot carve {} uplink /30s from transit_prefix {:?}",
                    uplink_ports.len(),
                    n.transit_prefix
                ))
            })?;
            (IpAddr::V4(f), IpAddr::V4(l))
        }
        // Unused in Bgp mode, but the field is required.
        RouterMode::Bgp => (
            IpAddr::V6(Ipv6Addr::UNSPECIFIED),
            IpAddr::V6(Ipv6Addr::UNSPECIFIED),
        ),
    };

    let rack_network_config = RackNetworkConfig {
        rack_subnet: parse(&n.rack_subnet, "rack_subnet")?,
        infra_ip_first: infra_first,
        infra_ip_last: infra_last,
        ports,
        // BGP config only in Bgp mode; Static mode carries no [[bgp]].
        bgp: match n.router_mode {
            RouterMode::Bgp => vec![BgpConfig {
                asn: n.bgp_asn,
                originate: vec![parse(&n.infra_prefix, "infra_prefix")?],
                shaper: None,
                checker: None,
                max_paths: 1,
            }],
            RouterMode::Static => vec![],
        },
        // Static mode optionally tracks each uplink gateway with single-hop BFD.
        bfd: match (n.router_mode, n.transit_bfd) {
            (RouterMode::Static, true) => uplink_ports
                .iter()
                .map(|p| {
                    Ok(BfdPeerConfig {
                        // The sidecar's own /30 address is the BFD listen/source.
                        // Without it, early networking programs listen=0.0.0.0
                        // and mgd rejects the peer, so the session never
                        // establishes and the router's BFD-tracked route (hence
                        // time sync) hangs.
                        local: Some(parse(
                            p.sidecar_addr.split('/').next().unwrap_or(&p.sidecar_addr),
                            "bfd local",
                        )?),
                        remote: parse(&p.gateway, "bfd gateway")?,
                        detection_threshold: 3,
                        required_rx: 1_000_000,
                        mode: BfdMode::SingleHop,
                        switch: switch_slot(&p.switch),
                    })
                })
                .collect::<Result<Vec<_>>>()?,
            _ => vec![],
        },
    };

    let dns_servers = n
        .dns_servers
        .iter()
        .map(|s| parse(s, "dns_servers"))
        .collect::<Result<_>>()?;
    let external_dns_ips = n
        .external_dns_ips
        .iter()
        .map(|s| parse(s, "external_dns_ips"))
        .collect::<Result<_>>()?;

    let silo = &cfg.recovery_silo;
    Ok(RackInitializeRequest {
        trust_quorum_peers: Some(trust_quorum_peers),
        bootstrap_discovery: BootstrapAddressDiscovery::OnlyThese {
            addrs: bootstrap_addrs,
        },
        ntp_servers: n.ntp_servers.clone(),
        dns_servers,
        internal_services_ip_pool_ranges: vec![pool],
        external_dns_ips,
        external_dns_zone_name: n.dns_zone.clone(),
        external_certificates: vec![],
        recovery_silo: RecoverySiloConfig {
            silo_name: silo.silo_name.clone(),
            user_name: silo.user_name.clone(),
            user_password_hash: silo.user_password_hash.clone(),
        },
        rack_network_config,
        allowed_source_ips: AllowedSourceIps::Any,
        // Added in omicron v20 (a3fee0ec). Opt-in jumbo frames on external
        // networking; off for the lab default.
        external_jumbo_frames_opt_in_enabled: false,
    })
}

impl VoxelConfig {
    /// Render this config's `config-rss.toml` for `rack` (0-based).
    pub fn to_config_rss(&self, rack: usize) -> Result<String> {
        let req = request_from_config(self, rack)?;
        toml::to_string(&req).map_err(|e| RssConfigError(format!("serialize config-rss.toml: {e}")))
    }
}
