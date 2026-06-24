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
    BgpConfig, BgpPeerConfig, LinkFec, LinkSpeed, LldpAdminStatus, LldpPortConfig,
    MaxPathConfig, PortConfig, RackNetworkConfig, RouterLifetimeConfig, RouterPeerType,
    SwitchSlot, UplinkAddress, UplinkAddressConfig,
};
use voxel_config::{UplinkCfg, VoxelConfig};

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
        (IpAddr::V4(a), IpAddr::V4(b)) => {
            Ok(IpRange::V4(Ipv4Range::new(a, b).map_err(|e| anyhow::anyhow!("pool: {e}"))?))
        }
        (IpAddr::V6(a), IpAddr::V6(b)) => {
            Ok(IpRange::V6(Ipv6Range::new(a, b).map_err(|e| anyhow::anyhow!("pool: {e}"))?))
        }
        _ => bail!("pool first/last must be the same address family"),
    }
}

fn uplink_port(u: &UplinkCfg) -> Result<PortConfig> {
    Ok(PortConfig {
        routes: vec![],
        addresses: vec![UplinkAddressConfig { address: UplinkAddress::AddrConf, vlan_id: None }],
        switch: switch_slot(&u.switch),
        port: u.port.clone(),
        uplink_port_speed: link_speed(&u.port_speed),
        uplink_port_fec: Some(LinkFec::None),
        bgp_peers: vec![BgpPeerConfig {
            asn: u.peer_asn,
            port: u.port.clone(),
            addr: RouterPeerType::Unnumbered {
                router_lifetime: RouterLifetimeConfig::new(u.router_lifetime)
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
        autoneg: false,
        lldp: Some(LldpPortConfig {
            status: LldpAdminStatus::Enabled,
            chassis_id: Some(u.switch.clone()),
            port_id: None,
            port_description: Some(u.lldp_port_description.clone()),
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

    let pool = ip_range(&n.service_pool_first, &n.service_pool_last)?;

    let mut ports = Vec::new();
    for u in &n.uplinks {
        ports.push(uplink_port(u)?);
    }

    let rack_network_config = RackNetworkConfig {
        rack_subnet: n.rack_subnet.parse().context("rack_subnet")?,
        infra_ip_first: "::".parse::<IpAddr>()?,
        infra_ip_last: "::".parse::<IpAddr>()?,
        ports,
        bgp: vec![BgpConfig {
            asn: n.bgp_asn,
            originate: vec![n.infra_prefix.parse().context("infra_prefix")?],
            shaper: None,
            checker: None,
            max_paths: MaxPathConfig::default(),
        }],
        bfd: vec![],
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
        trust_quorum_peers: None,
        bootstrap_discovery: BootstrapAddressDiscovery::OnlyThese { addrs: bootstrap_addrs },
        ntp_servers: n.ntp_servers.clone(),
        dns_servers,
        internal_services_ip_pool_ranges: vec![pool],
        external_dns_ips,
        external_dns_zone_name: n.dns_zone.clone(),
        external_certificates: vec![],
        recovery_silo: RecoverySiloConfig {
            silo_name: silo.silo_name.parse().map_err(|e| anyhow::anyhow!("silo_name: {e}"))?,
            user_name: silo.user_name.parse().map_err(|e| anyhow::anyhow!("user_name: {e}"))?,
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
            let path = args.next().context("usage: voxel-rss-gen validate <config-rss.toml>")?;
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
            let path = args.next().context("usage: voxel-rss-gen canon <config-rss.toml>")?;
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
