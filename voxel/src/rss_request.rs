//! Builds omicron RackInitializeRequest values from a VoxelConfig.

use std::collections::BTreeSet;
use std::net::{IpAddr, Ipv6Addr};

use anyhow::{Context, Result, bail};
use rack_init_config::{
    AllowedSourceIps, BaseboardId, BfdMode, BfdPeerConfig, BgpConfig, BgpPeerConfig,
    BootstrapAddressDiscovery, IdOrdMap, IpRange, Ipv4Range, Ipv6Range, LinkFec, LinkSpeed,
    LldpAdminStatus, LldpPortConfig, MaxPathConfig, PortConfig, RackInitializeRequest,
    RackNetworkConfig, RecoverySiloConfig, RouteConfig, RouterLifetimeConfig, RouterPeerType,
    ServiceIpPoolConfig, SwitchSlot, UplinkAddress, UplinkAddressConfig, UplinkPorts,
};
use voxel_config::{RouterMode, UplinkPort, VoxelConfig};

// Well-known service pool identity, matching omicron's v1-to-v2 conversion.
const SERVICE_POOL_NAME: &str = "oxide-service-pool-v4";
const SERVICE_POOL_DESCRIPTION: &str = "IPv4 IP Pool for Oxide Services";

fn switch_slot(name: &str) -> Result<SwitchSlot> {
    match name {
        "switch0" => Ok(SwitchSlot::Switch0),
        "switch1" => Ok(SwitchSlot::Switch1),
        other => bail!("unknown switch {other:?} (expected switch0/switch1)"),
    }
}

fn link_speed(speed: &str) -> Result<LinkSpeed> {
    match speed {
        "0G" => Ok(LinkSpeed::Speed0G),
        "1G" => Ok(LinkSpeed::Speed1G),
        "10G" => Ok(LinkSpeed::Speed10G),
        "25G" => Ok(LinkSpeed::Speed25G),
        "40G" => Ok(LinkSpeed::Speed40G),
        "50G" => Ok(LinkSpeed::Speed50G),
        "100G" => Ok(LinkSpeed::Speed100G),
        "200G" => Ok(LinkSpeed::Speed200G),
        "400G" => Ok(LinkSpeed::Speed400G),
        other => bail!("unknown link speed {other:?}"),
    }
}

/// An IpRange from first/last strings of the same address family.
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

fn lldp(switch: &str, port_description: &str) -> LldpPortConfig {
    LldpPortConfig {
        status: LldpAdminStatus::Enabled,
        chassis_id: Some(switch.to_string()),
        port_id: None,
        port_description: Some(port_description.to_string()),
        system_name: None,
        system_description: None,
        management_addrs: None,
    }
}

/// One uplink port toward a single fabric router. Static mode uses the port's
/// numbered /30 and a default route; Bgp mode an unnumbered eBGP peer.
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
            vec![RouteConfig {
                destination: "0.0.0.0/0".parse().context("default route")?,
                nexthop: p.gateway.parse().context("uplink gateway")?,
                vlan_id: None,
                rib_priority: None,
            }],
            vec![UplinkAddressConfig {
                address: UplinkAddress::Static {
                    ip_net: p.sidecar_addr.parse().context("uplink /30")?,
                },
                vlan_id: None,
            }],
            vec![],
        ),
    };
    Ok(PortConfig {
        routes,
        addresses,
        switch: switch_slot(&p.switch)?,
        port: p.port.clone(),
        uplink_port_speed: link_speed(&p.port_speed)?,
        uplink_port_fec: Some(LinkFec::None),
        bgp_peers,
        autoneg: false,
        lldp: Some(lldp(&p.switch, &p.lldp)),
        tx_eq: None,
    })
}

/// A cross-rack sidecar interconnect port: link-local (addrconf) so mg-ddm can
/// peer over it, no routes or BGP, 100G to match the sidecar rear-port links.
fn interconnect_port(switch: &str, port: &str) -> Result<PortConfig> {
    Ok(PortConfig {
        routes: vec![],
        addresses: vec![UplinkAddressConfig {
            address: UplinkAddress::AddrConf,
            vlan_id: None,
        }],
        switch: switch_slot(switch)?,
        port: port.to_string(),
        uplink_port_speed: LinkSpeed::Speed100G,
        uplink_port_fec: Some(LinkFec::None),
        bgp_peers: vec![],
        autoneg: false,
        lldp: Some(lldp(switch, &format!("interconnect-{port}"))),
        tx_eq: None,
    })
}

/// Build the request for a single rack (0-based) of a voxel config. Each rack
/// is an independent RSS domain: the bootstrap set is filtered to that rack's
/// sleds and the customer/service network is offset per rack.
pub fn request_from_config(cfg: &VoxelConfig, rack: usize) -> Result<RackInitializeRequest> {
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

    let pool = ServiceIpPoolConfig::new(
        SERVICE_POOL_NAME
            .parse()
            .map_err(|e| anyhow::anyhow!("service pool name: {e}"))?,
        SERVICE_POOL_DESCRIPTION.to_string(),
        vec![ip_range(&n.service_pool_first, &n.service_pool_last)?],
    )
    .context("service pool")?;
    let service_ip_pools = IdOrdMap::from_iter_unique([pool]).context("service pools")?;

    let uplink_ports = cfg.uplink_ports(rack);
    let mut ports = Vec::new();
    for p in &uplink_ports {
        ports.push(uplink_port(p, n.router_mode)?);
    }
    for (sw, port) in cfg.interconnect_ports(rack) {
        ports.push(interconnect_port(&sw, &port)?);
    }
    let ports =
        UplinkPorts::new(ports).context("rack network config needs at least one uplink port")?;

    // Static-mode uplinks are numbered /30s validated against this lot at
    // handoff; Bgp uplinks are unnumbered, so the lot is unused.
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
        // Static mode optionally tracks each uplink gateway with single-hop
        // BFD. The sidecar's own /30 address is the listen/source; without it
        // mgd rejects the peer and time sync hangs.
        bfd: match (n.router_mode, n.transit_bfd) {
            (RouterMode::Static, true) => uplink_ports
                .iter()
                .map(|p| {
                    Ok(BfdPeerConfig {
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
                        switch: switch_slot(&p.switch)?,
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
        service_ip_pools,
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
        external_jumbo_frames_opt_in_enabled: false,
    })
}

/// Render a rack's config-rss.toml.
pub fn config_rss_toml(cfg: &VoxelConfig, rack: usize) -> Result<String> {
    let request = request_from_config(cfg, rack)?;
    rack_init_config::to_config_rss_toml(&request).context("serialize RackInitializeRequest")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(racks: usize, network: &str) -> VoxelConfig {
        let text = format!("[topology]\nracks = {racks}\nsleds = 3\n\n[network]\n{network}\n");
        VoxelConfig::from_toml(&text).expect("parse test config")
    }

    #[test]
    fn static_bfd_ports_and_peers() {
        let cfg = config(1, "router_mode = \"static\"\ntransit_bfd = true");
        let req = request_from_config(&cfg, 0).unwrap();
        let net = &req.rack_network_config;
        assert!(net.bgp.is_empty());
        let port = net.ports.first();
        assert!(port.bgp_peers.is_empty());
        assert_eq!(port.routes.len(), 1);
        let UplinkAddress::Static { ip_net } = port.addresses[0].address else {
            panic!("static mode must number the uplink");
        };
        // One BFD session per uplink, listening on the sidecar's own address.
        assert_eq!(net.bfd.len(), net.ports.len());
        assert_eq!(net.bfd[0].local, Some(oxnet::IpNet::from(ip_net).addr()));
        assert_ne!(net.infra_ip_first, "::".parse::<IpAddr>().unwrap());
    }

    #[test]
    fn bgp_ports_and_peers() {
        let cfg = config(1, "router_mode = \"bgp\"");
        let req = request_from_config(&cfg, 0).unwrap();
        let net = &req.rack_network_config;
        assert_eq!(net.bgp.len(), 1);
        assert!(net.bfd.is_empty());
        let port = net.ports.first();
        assert!(port.routes.is_empty());
        assert_eq!(port.addresses[0].address, UplinkAddress::AddrConf);
        assert!(matches!(
            port.bgp_peers[0].addr,
            RouterPeerType::Unnumbered { .. }
        ));
        assert_eq!(net.infra_ip_first, "::".parse::<IpAddr>().unwrap());
    }

    #[test]
    fn multirack_projects_rack1() {
        let cfg = config(2, "router_mode = \"bgp\"");
        let rack0 = request_from_config(&cfg, 0).unwrap();
        let rack1 = request_from_config(&cfg, 1).unwrap();
        // Each rack is an independent RSS domain: disjoint bootstrap and trust
        // quorum sets, offset ASN and rack subnet, interconnect ports present.
        let serials = |r: &RackInitializeRequest| {
            r.trust_quorum_peers
                .as_ref()
                .unwrap()
                .iter()
                .map(|b| b.serial_number.clone())
                .collect::<std::collections::BTreeSet<_>>()
        };
        assert!(serials(&rack0).is_disjoint(&serials(&rack1)));
        let BootstrapAddressDiscovery::OnlyThese { addrs } = &rack1.bootstrap_discovery else {
            panic!("bootstrap discovery must list rack 1's sleds");
        };
        assert_eq!(addrs.len(), 3);
        assert_ne!(
            rack0.rack_network_config.rack_subnet,
            rack1.rack_network_config.rack_subnet
        );
        assert_ne!(
            rack0.rack_network_config.bgp[0].asn,
            rack1.rack_network_config.bgp[0].asn
        );
        assert_eq!(
            rack1.rack_network_config.ports.len(),
            cfg.uplink_ports(1).len() + cfg.interconnect_ports(1).len()
        );
    }
}
