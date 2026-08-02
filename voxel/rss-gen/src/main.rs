//! Typed `config-rss.toml` validator + generator, built against the IMAGE's own
//! omicron source (path dep to `/opt/omicron`, per publish pinned to a commit).
//!
//! `validate <file>` deserializes config-rss into omicron's real
//! `RackInitializeRequest` - schema errors surface in milliseconds instead of
//! via 8-minute sled-agent launches.
//!
//! `generate [out.toml]` does the inverse: it builds a `RackInitializeRequest`
//! from a voxel topology and serializes it, so the config is release-accurate by
//! construction (no hand-rolled TOML). Output defaults to stdout.
//!
//! This is the seed of `voxel-init` - eventually baked into the image so the
//! rack generates its own RSS config from a voxel descriptor at boot.

use std::collections::BTreeSet;
use std::net::{IpAddr, Ipv6Addr};

use anyhow::{bail, Context, Result};
use bootstrap_agent_lockstep_types::{
    BootstrapAddressDiscovery, RackInitializeRequest, RecoverySiloConfig,
};
use omicron_common::address::{IpRange, Ipv4Range, Ipv6Range};
use omicron_common::api::external::AllowedSourceIps;
use sled_agent_types::early_networking::{
    BfdMode, BfdPeerConfig, BgpConfig, BgpPeerConfig, LinkFec, LinkSpeed, LldpAdminStatus,
    LldpPortConfig, MaxPathConfig, PortConfig, RackNetworkConfig, RouteConfig,
    RouterLifetimeConfig, RouterPeerType, SwitchSlot, UplinkAddress, UplinkAddressConfig,
};
use sled_hardware_types::BaseboardId;

// Newer omicron wraps the uplink port list in a non-empty `UplinkPorts` newtype;
// v20-era omicron uses a bare `Vec<PortConfig>`. build.rs sets `has_uplink_ports`
// when the type exists in the pinned omicron, so one source builds for any era.
#[cfg(has_uplink_ports)]
use sled_agent_types::early_networking::UplinkPorts;
use voxel_config::{RouterMode, UplinkPort, VoxelConfig};

fn switch_slot(s: &str) -> SwitchSlot {
    match s {
        "switch1" => SwitchSlot::Switch1,
        _ => SwitchSlot::Switch0,
    }
}

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

/// `IpRange` from `first`/`last` strings of the same address family.
fn ip_range(first: &str, last: &str) -> Result<IpRange> {
    let f: IpAddr = first.parse().context("pool first")?;
    let l: IpAddr = last.parse().context("pool last")?;
    match (f, l) {
        (IpAddr::V4(a), IpAddr::V4(b)) => Ok(IpRange::V4(
            Ipv4Range::new(a, b).map_err(|e| anyhow::anyhow!("pool: {e}"))?,
        )),
        (IpAddr::V6(a), IpAddr::V6(b)) => Ok(IpRange::V6(
            Ipv6Range::new(a, b).map_err(|e| anyhow::anyhow!("pool: {e}"))?,
        )),
        _ => bail!("pool first/last must be the same address family"),
    }
}

/// One uplink port toward a single fabric router (`UplinkPort` carries the
/// 2-way fanout: one port per router per switch). `Static` mode uses the port's
/// numbered /30 + default route; `Bgp` mode an unnumbered eBGP peer.
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
                    router_lifetime: RouterLifetimeConfig::new(p.router_lifetime)
                        .map_err(|e| anyhow::anyhow!("router_lifetime: {e}"))?,
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
                allowed_import: Default::default(),
                allowed_export: Default::default(),
                vlan_id: None,
            }],
        ),
        RouterMode::Static => (
            // Default route upstream via this router.
            vec![RouteConfig {
                destination: "0.0.0.0/0".parse().context("default route")?,
                nexthop: p.gateway.parse().context("uplink gateway")?,
                vlan_id: None,
                rib_priority: None,
            }],
            // Numbered /30 on the sidecar side.
            vec![UplinkAddressConfig {
                address: UplinkAddress::Static {
                    ip_net: p.sidecar_addr.parse().context("uplink /30")?,
                },
                vlan_id: None,
            }],
            // No BGP; BFD tracks the nexthop (rack_network_config.bfd).
            vec![],
        ),
    };
    Ok(PortConfig {
        routes,
        addresses,
        switch: switch_slot(&p.switch),
        port: p.port.clone(),
        uplink_port_speed: link_speed(&p.port_speed),
        uplink_port_fec: Some(LinkFec::None),
        bgp_peers,
        autoneg: false,
        lldp: Some(LldpPortConfig {
            status: LldpAdminStatus::Enabled,
            chassis_id: Some(p.switch.clone()),
            port_id: None,
            port_description: Some(p.lldp.clone()),
            system_name: None,
            system_description: None,
            management_addrs: None,
        }),
        tx_eq: None,
    })
}

/// A cross-rack sidecar interconnect port: link-local (`AddrConf`), no routes or
/// BGP - a "cluster port" for the multirack underlay. DDM (if it runs on the port)
/// carries the shared-/48 across the mesh. 100G to match the sidecar<->sidecar
/// rear-port links.
fn interconnect_port(switch: &str, port: &str) -> Result<PortConfig> {
    Ok(PortConfig {
        routes: vec![],
        // Link-local via addrconf so mg-ddm can peer cross-rack over the
        // interconnect. Nexus lot-validates this against the infra lot; the v6
        // block added to that lot (nexus rack.rs, build-cp patch 1c) lets it
        // reserve in Static mode as it already does in BGP mode.
        addresses: vec![UplinkAddressConfig {
            address: UplinkAddress::AddrConf,
            vlan_id: None,
        }],
        switch: switch_slot(switch),
        port: port.to_string(),
        uplink_port_speed: link_speed("100G"),
        uplink_port_fec: Some(LinkFec::None),
        bgp_peers: vec![],
        autoneg: false,
        lldp: Some(LldpPortConfig {
            status: LldpAdminStatus::Enabled,
            chassis_id: Some(switch.to_string()),
            port_id: None,
            port_description: Some(format!("interconnect-{port}")),
            system_name: None,
            system_description: None,
            management_addrs: None,
        }),
        tx_eq: None,
    })
}

/// Build a typed `RackInitializeRequest` for a single rack (`rack`, 0-based) of a
/// voxel config. Multi-rack deployments call this once per rack: each rack is an
/// independent RSS domain, so the bootstrap set is filtered to that rack's sleds
/// and the customer/service network is offset by `Network::for_rack`.
fn request_from_config(cfg: &VoxelConfig, rack: usize) -> Result<RackInitializeRequest> {
    let n = &cfg.network.for_rack(rack);

    let bootstrap_addrs: BTreeSet<Ipv6Addr> = cfg
        .sleds()
        .iter()
        .filter(|s| s.rss && s.rack == rack)
        .map(|s| s.bootstrap_addr().parse())
        .collect::<std::result::Result<_, _>>()
        .context("bootstrap addrs")?;

    let trust_quorum_peers: Vec<BaseboardId> = cfg
        .sleds()
        .iter()
        .filter(|s| s.rss && s.rack == rack)
        .map(|s| BaseboardId {
            serial_number: s.serial_number.clone(),
            part_number: s.part_number.clone(),
        })
        .collect();

    let pool = ip_range(&n.service_pool_first, &n.service_pool_last)?;

    // 2-way fanout: one port per fabric router per switch (qsfp0, qsfp1, ...).
    let uplink_ports = cfg.uplink_ports(rack);
    let mut ports = Vec::new();
    for p in &uplink_ports {
        ports.push(uplink_port(p, n.router_mode)?);
    }
    // Cross-rack sidecar interconnect ports: link-local (AddrConf) cluster ports,
    // no routes/BGP, so DDM can carry the shared-/48 underlay across the mesh.
    for (sw, port) in cfg.interconnect_ports(rack) {
        ports.push(interconnect_port(&sw, &port)?);
    }
    // Newer omicron requires the non-empty `UplinkPorts` newtype; older takes the
    // bare Vec. Rebind per era (build.rs cfg); `ports` gets the right type either way.
    #[cfg(has_uplink_ports)]
    let ports = UplinkPorts::new(ports)
        .map_err(|_| anyhow::anyhow!("rack network config needs at least one uplink port"))?;

    // Static-mode uplinks are numbered /30s; Nexus validates each switch-port
    // address against this lot at handoff. Derived from transit_prefix to cover
    // the sidecar /30s. Bgp uplinks are unnumbered, so the lot is unused.
    let (infra_first, infra_last): (IpAddr, IpAddr) = match n.router_mode {
        RouterMode::Static => {
            let (f, l) = n
                .infra_ip_range(uplink_ports.len())
                .context("infra_ip_range from transit_prefix")?;
            (IpAddr::V4(f), IpAddr::V4(l))
        }
        RouterMode::Bgp => ("::".parse()?, "::".parse()?),
    };

    let rack_network_config = RackNetworkConfig {
        rack_subnet: n.rack_subnet.parse().context("rack_subnet")?,
        infra_ip_first: infra_first,
        infra_ip_last: infra_last,
        ports,
        // BGP config only in Bgp mode; Static mode carries no [[bgp]].
        bgp: match n.router_mode {
            RouterMode::Bgp => vec![BgpConfig {
                asn: n.bgp_asn,
                originate: vec![n.infra_prefix.parse().context("infra_prefix")?],
                shaper: None,
                checker: None,
                max_paths: MaxPathConfig::default(),
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
                        // Without it, early networking programs listen=0.0.0.0 and
                        // mgd rejects the peer, so the session never establishes and
                        // the router's BFD-tracked route (hence time sync) hangs.
                        local: Some(
                            p.sidecar_addr
                                .split('/')
                                .next()
                                .unwrap_or(&p.sidecar_addr)
                                .parse()
                                .context("bfd local")?,
                        ),
                        remote: p.gateway.parse().context("bfd gateway")?,
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
        .map(|s| s.parse())
        .collect::<std::result::Result<_, _>>()
        .context("dns_servers")?;
    let external_dns_ips = n
        .external_dns_ips
        .iter()
        .map(|s| s.parse())
        .collect::<std::result::Result<_, _>>()
        .context("external_dns_ips")?;

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
            silo_name: silo
                .silo_name
                .parse()
                .map_err(|e| anyhow::anyhow!("silo_name: {e}"))?,
            user_name: silo
                .user_name
                .parse()
                .map_err(|e| anyhow::anyhow!("user_name: {e}"))?,
            user_password_hash: silo
                .user_password_hash
                .parse()
                .map_err(|e| anyhow::anyhow!("user_password_hash: {e}"))?,
        },
        rack_network_config,
        allowed_source_ips: AllowedSourceIps::Any,
        // Added in omicron v20 (a3fee0ec). Opt-in jumbo frames on external
        // networking; off for the lab default.
        external_jumbo_frames_opt_in_enabled: false,
    })
}

fn validate(path: &str) -> Result<()> {
    let text = std::fs::read_to_string(path)?;
    let req: RackInitializeRequest = toml::from_str(&text)?;
    println!("OK: config-rss parses as RackInitializeRequest");
    println!("  bootstrap discovery: {:?}", req.bootstrap_discovery);
    println!("  external_dns_zone: {}", req.external_dns_zone_name);
    Ok(())
}

/// Deserialize a config-rss into the typed request and re-serialize it
/// canonically - lets us prove two configs are *semantically* identical by
/// diffing their canonical forms (layout/normalization aside).
fn canon(path: &str) -> Result<()> {
    let text = std::fs::read_to_string(path)?;
    let req: RackInitializeRequest = toml::from_str(&text)?;
    print!("{}", toml::to_string(&req)?);
    Ok(())
}

fn generate(config_path: &str, out: Option<&str>, rack: usize) -> Result<()> {
    let cfg = VoxelConfig::from_toml(&std::fs::read_to_string(config_path)?)
        .map_err(|e| anyhow::anyhow!("parse {config_path}: {e}"))?;
    let req = request_from_config(&cfg, rack)?;
    let text = toml::to_string(&req).context("serialize RackInitializeRequest")?;
    match out {
        Some(path) => {
            std::fs::write(path, &text)?;
            eprintln!("wrote {path}");
        }
        None => print!("{text}"),
    }
    Ok(())
}

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("validate") => {
            let path = args
                .next()
                .context("usage: voxel-rss-gen validate <config-rss.toml>")?;
            validate(&path)
        }
        Some("generate") => {
            // generate <voxel.toml> [out.toml] [--rack R]
            let mut positional: Vec<String> = Vec::new();
            let mut rack = 0usize;
            while let Some(a) = args.next() {
                if a == "--rack" {
                    let r = args.next().context("--rack needs a value")?;
                    rack = r.parse().context("--rack must be a non-negative integer")?;
                } else {
                    positional.push(a);
                }
            }
            let cfg = positional
                .first()
                .context("usage: voxel-rss-gen generate <voxel.toml> [out.toml] [--rack R]")?;
            generate(cfg, positional.get(1).map(String::as_str), rack)
        }
        Some("canon") => {
            let path = args
                .next()
                .context("usage: voxel-rss-gen canon <config-rss.toml>")?;
            canon(&path)
        }
        other => {
            anyhow::bail!(
                "usage: voxel-rss-gen <validate <config-rss.toml> | \
                 generate <voxel.toml> [out.toml] [--rack R] | \
                 canon <config-rss.toml>> (got {other:?})"
            )
        }
    }
}
