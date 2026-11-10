// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Drive rack setup through wicketd's commission API. The typed request body
//! is built here; the API is a versioned interface, so it is reached with its
//! generated progenitor client over an ssh tunnel to its in-zone loopback.

use anyhow::{Context, Result, anyhow, bail};
use camino::{Utf8Path, Utf8PathBuf};
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::net::IpAddr;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use voxel_config::{RouterMode, UplinkPort, VoxelConfig};
use wicketd_commission_client::Client;
use wicketd_commission_types_versions::latest::inventory;
use wicketd_commission_types_versions::latest::rack_setup as types;

use crate::net::{
    EPHEMERAL_HOST_OPTS, PASSWORD_AUTH_OPTS, ensure_askpass, ssh_capture,
    zlogin,
};

/// wicketd commission API port, bound on the switch zone's loopback only.
const COMMISSION_PORT: u16 = 12234;

/// Bootstrap-network addresses begin with this hextet (voxel_config's
/// BOOTSTRAP_NET_PREFIX); the switch zone's is the tunnel target.
const BOOTSTRAP_ADDR_PREFIX: &str = "fdb0";

/// This rack's RSS sleds, by cubby slot (each sled's global index). wicketd
/// correlates these slots with the SPs it discovers.
pub(crate) fn bootstrap_slots(cfg: &VoxelConfig, rack: usize) -> BTreeSet<u16> {
    cfg.sleds()
        .iter()
        .filter(|s| s.rss && s.rack == rack)
        .map(|s| s.index as u16)
        .collect()
}

fn link_speed(speed: &str) -> Result<types::LinkSpeed> {
    match speed {
        "0G" => Ok(types::LinkSpeed::Speed0G),
        "1G" => Ok(types::LinkSpeed::Speed1G),
        "10G" => Ok(types::LinkSpeed::Speed10G),
        "25G" => Ok(types::LinkSpeed::Speed25G),
        "40G" => Ok(types::LinkSpeed::Speed40G),
        "50G" => Ok(types::LinkSpeed::Speed50G),
        "100G" => Ok(types::LinkSpeed::Speed100G),
        "200G" => Ok(types::LinkSpeed::Speed200G),
        "400G" => Ok(types::LinkSpeed::Speed400G),
        other => bail!("unknown link speed {other:?}"),
    }
}

fn lldp(switch: &str, port_description: &str) -> types::LldpPortConfig {
    types::LldpPortConfig {
        status: types::LldpAdminStatus::Enabled,
        chassis_id: Some(switch.to_string()),
        port_id: None,
        port_description: Some(port_description.to_string()),
        system_name: None,
        system_description: None,
        management_addrs: None,
    }
}

/// One uplink port in wicket's user-specified form; same shape as the
/// config-rss uplink (rss_request::uplink_port) per router mode.
fn uplink_port(
    p: &UplinkPort,
    mode: RouterMode,
) -> Result<types::UserSpecifiedPortConfig> {
    let (routes, addresses, bgp_peers) = match mode {
        RouterMode::Bgp => (
            vec![],
            vec![types::UserSpecifiedUplinkAddressConfig {
                address: types::UplinkAddress::AddrConf,
                vlan_id: None,
            }],
            vec![types::UserSpecifiedBgpPeerConfig {
                asn: p.peer_asn,
                port: p.port.clone(),
                addr: types::UserSpecifiedRouterPeerAddr::Unnumbered,
                src_addr: None,
                router_lifetime: types::RouterLifetimeConfig::new(
                    p.router_lifetime,
                )
                .map_err(|e| anyhow!("router_lifetime: {e}"))?,
                hold_time: None,
                idle_hold_time: None,
                delay_open: None,
                connect_retry: None,
                keepalive: None,
                remote_asn: None,
                min_ttl: None,
                auth_key_id: None,
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
            vec![types::RouteConfig {
                destination: "0.0.0.0/0".parse().context("default route")?,
                nexthop: p.gateway.parse().context("uplink gateway")?,
                vlan_id: None,
                rib_priority: None,
            }],
            vec![types::UserSpecifiedUplinkAddressConfig {
                address: types::UplinkAddress::Static {
                    ip_net: p.sidecar_addr.parse().context("uplink /30")?,
                },
                vlan_id: None,
            }],
            vec![],
        ),
    };
    Ok(types::UserSpecifiedPortConfig::Uplink(types::UplinkPortConfig {
        routes,
        addresses,
        uplink_port_speed: link_speed(&p.port_speed)?,
        uplink_port_fec: Some(types::LinkFec::None),
        autoneg: false,
        bgp_peers,
        lldp: Some(lldp(&p.switch, &p.lldp)),
        tx_eq: None,
    }))
}

/// A cross-rack interconnect port to run DDM over.
fn interconnect_port(
    switch: &str,
    port: &str,
) -> types::UserSpecifiedPortConfig {
    types::UserSpecifiedPortConfig::Ddm(types::L1PortConfig {
        speed: types::LinkSpeed::Speed100G,
        fec: Some(types::LinkFec::None),
        autoneg: false,
        lldp: Some(lldp(switch, &format!("interconnect-{port}"))),
        tx_eq: None,
    })
}

/// Build the commission rack-setup config for `rack` from a voxel config;
/// field derivations mirror rss_request::request_from_config.
pub(crate) fn rss_config(
    cfg: &VoxelConfig,
    rack: usize,
) -> Result<types::PutRssUserConfigInsensitive> {
    let n = &cfg.network.for_rack(rack);

    let pool = types::ServiceIpPoolConfig::new(
        "oxide-service-pool-v4"
            .parse()
            .map_err(|e| anyhow!("service pool name: {e}"))?,
        "IPv4 IP Pool for Oxide Services".to_string(),
        vec![ip_range(&n.service_pool_first, &n.service_pool_last)?],
    )
    .context("service pool")?;
    let service_ip_pools = rack_init_config::IdOrdMap::from_iter_unique([pool])
        .map_err(|e| anyhow!("service pools: {e}"))?;

    let mut switch0 = BTreeMap::new();
    let mut switch1 = BTreeMap::new();
    let mut insert = |switch: &str, port: String, c| -> Result<()> {
        match switch {
            "switch0" => switch0.insert(port, c),
            "switch1" => switch1.insert(port, c),
            other => bail!("unknown switch {other:?}"),
        };
        Ok(())
    };
    for p in cfg.uplink_ports(rack) {
        insert(&p.switch, p.port.clone(), uplink_port(&p, n.router_mode)?)?;
    }
    for (sw, port) in cfg.interconnect_ports(rack) {
        insert(&sw, port.clone(), interconnect_port(&sw, &port))?;
    }

    let (infra_first, infra_last): (IpAddr, IpAddr) = match n.router_mode {
        RouterMode::Static => {
            let (f, l) = n
                .infra_ip_range(cfg.uplink_ports(rack).len())
                .context("infra_ip_range from transit_prefix")?;
            (IpAddr::V4(f), IpAddr::V4(l))
        }
        RouterMode::Bgp => ("::".parse()?, "::".parse()?),
    };

    let rack_network_config = types::UserSpecifiedRackNetworkConfig {
        rack_subnet_address: None,
        infra_ip_first: infra_first,
        infra_ip_last: infra_last,
        switch0,
        switch1,
        bgp: match n.router_mode {
            RouterMode::Bgp => vec![types::BgpConfig {
                asn: n.bgp_asn,
                originate: vec![
                    n.infra_prefix.parse().context("infra_prefix")?,
                ],
                shaper: None,
                checker: None,
                max_paths: Default::default(),
            }],
            RouterMode::Static => vec![],
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

    Ok(types::PutRssUserConfigInsensitive {
        bootstrap_sleds: bootstrap_slots(cfg, rack),
        ntp_servers: n.ntp_servers.clone(),
        dns_servers,
        service_ip_pools,
        external_dns_ips,
        external_dns_zone_name: n.dns_zone.clone(),
        rack_network_config,
        allowed_source_ips: types::AllowedSourceIps::Any,
        external_jumbo_frames_opt_in_enabled: true,
    })
}

/// Drive rack setup for `rack` through the commission API: wait for the
/// rack's bootstrap sleds, upload the config, a self-signed cert pair, and
/// the recovery password, then start RSS. The caller watches RSS as usual.
pub(crate) async fn drive(
    cfg: &VoxelConfig,
    d: &libfalcon::Runner,
    scrimlet: libfalcon::NodeRef,
    scrimlet_name: &str,
    rack: usize,
    tag: &str,
) -> Result<()> {
    let (_tunnel, client) =
        connect_client(cfg, d, scrimlet, scrimlet_name, tag).await?;

    // Bootstrap sleds fill in as MGS discovers SPs and the bootstrap network
    // connects peers; poll until this rack's slots all report ready.
    let want = bootstrap_slots(cfg, rack);
    let deadline = Instant::now() + Duration::from_secs(300);
    loop {
        let have = ready_slots(&client).await.unwrap_or_default();
        if want.is_subset(&have) {
            slog::info!(d.log, "{tag}: {} bootstrap sleds ready", want.len());
            break;
        }
        if Instant::now() >= deadline {
            let detail = bootstrap_detail(&client)
                .await
                .unwrap_or_else(|e| format!("  (could not read it: {e:#})"));
            bail!(
                "bootstrap sleds not ready within 300s \
                 (have {have:?}, want {want:?}). wicketd's view:\n{detail}"
            );
        }
        tokio::time::sleep(Duration::from_secs(8)).await;
    }

    // The config PUT can transiently fail right after discovery while the SP
    // inventory settles; poll it under its own deadline.
    let body = rss_config(cfg, rack)?;
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        match client.put_rss_config(&body).await {
            Ok(_) => break,
            Err(e) if Instant::now() < deadline => {
                slog::info!(
                    d.log,
                    "{tag}: rack-setup config not accepted yet ({e}); retrying"
                );
                tokio::time::sleep(Duration::from_secs(6)).await;
            }
            Err(e) => bail!("put rack-setup config: {e}"),
        }
    }

    let net = cfg.network.for_rack(rack);
    let (cert, key) = gen_cert(&net.dns_zone, &rack_ca_path(rack))?;
    client
        .post_rss_config_cert(&types::CertificatePem(cert))
        .await
        .map_err(|e| anyhow!("upload cert: {e}"))?;
    client
        .post_rss_config_key(&types::PrivateKeyPem(key.into()))
        .await
        .map_err(|e| anyhow!("upload key: {e}"))?;
    client
        .put_rss_config_recovery_user_password_hash(
            &types::PutRecoveryUserPasswordHash {
                hash: types::NewPasswordHash(
                    cfg.recovery_silo.user_password_hash.clone(),
                ),
            },
        )
        .await
        .map_err(|e| anyhow!("set recovery password: {e}"))?;

    // The start can still race wicketd's own view of sled readiness, so
    // retry it under a deadline instead of failing on the first rejection.
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        match client.post_run_rack_setup().await {
            Ok(_) => break,
            Err(e) if Instant::now() < deadline => {
                slog::info!(
                    d.log,
                    "{tag}: rack setup not started yet ({e}); retrying"
                );
                tokio::time::sleep(Duration::from_secs(6)).await;
            }
            Err(e) => bail!("start rack setup: {e}"),
        }
    }
    slog::info!(d.log, "{tag}: commission-driven RSS started");
    Ok(())
}

/// The bootstrap-sled slots whose bootstrap IP the API already knows. A slot
/// can be listed before its advertisement lands (state ip null), and wicketd
/// rejects rack setup until every configured sled has an IP.
/// wicketd's whole bootstrap picture, one line per sled and peer: a slot is
/// ready only when its SP's baseboard matches a discovered bootstrap peer, so
/// a slot without an address next to an unmatched peer means the two report
/// different baseboards, and no peer at all means it was never discovered.
async fn bootstrap_detail(client: &Client) -> Result<String> {
    let resp = client
        .get_bootstrap_sleds()
        .await
        .map_err(|e| anyhow!("get bootstrap sleds: {e}"))?
        .into_inner();
    let mut out = String::new();
    for s in resp.sleds.iter() {
        let line = match &s.state {
            inventory::BootstrapSledState::Read { baseboard, ip } => format!(
                "SP baseboard {}/{}, bootstrap address {}",
                baseboard.part_number,
                baseboard.serial_number,
                ip.map_or("none (no matching peer)".to_string(), |ip| ip
                    .to_string())
            ),
            inventory::BootstrapSledState::NotRead => {
                "SP present, not read yet".to_string()
            }
            inventory::BootstrapSledState::Error { error } => {
                format!("SP read failed: {error:?}")
            }
        };
        out.push_str(&format!("  slot {}: {line}\n", s.id.slot));
    }
    for p in resp.unmatched_peers.iter() {
        out.push_str(&format!(
            "  unmatched peer {}: baseboard {}/{}\n",
            p.ip, p.baseboard.part_number, p.baseboard.serial_number
        ));
    }
    for p in resp.unidentified_peers.iter() {
        out.push_str(&format!("  unidentified peer: {p:?}\n"));
    }
    Ok(out)
}

async fn ready_slots(client: &Client) -> Result<BTreeSet<u16>> {
    let resp = client
        .get_bootstrap_sleds()
        .await
        .map_err(|e| anyhow!("get bootstrap sleds: {e}"))?
        .into_inner();
    Ok(resp
        .sleds
        .iter()
        .filter_map(|s| match &s.state {
            inventory::BootstrapSledState::Read { ip: Some(_), .. } => {
                Some(s.id.slot)
            }
            _ => None,
        })
        .collect())
}

/// The switch zone's bootstrap-network address, read from the scrimlet global
/// zone. The commission API binds only in-zone loopback, so the tunnel lands
/// on this address inside the zone.
fn zone_bootstrap_addr(gz_ip: &str) -> Result<String> {
    let cmd = zlogin(&format!(
        "ipadm show-addr -po addr | grep -o '{BOOTSTRAP_ADDR_PREFIX}[^/]*' \
         | head -1"
    ));
    ssh_capture(gz_ip, &cmd)
        .map(|o| o.trim().to_string())
        .filter(|a| !a.is_empty())
        .ok_or_else(|| anyhow!("no bootstrap address on oxz_switch"))
}

/// Connect to the scrimlet's commission API in wicketd through an ssh tunnel.
///
/// The returned [`Tunnel`] must outlive the client that forwards through it.
pub(crate) async fn connect_client(
    cfg: &VoxelConfig,
    d: &libfalcon::Runner,
    scrimlet: libfalcon::NodeRef,
    scrimlet_name: &str,
    tag: &str,
) -> Result<(Tunnel, Client)> {
    let gz_ip =
        crate::net::resolve_external_ip(cfg, d, scrimlet_name, scrimlet, false)
            .await
            .map_err(|e| anyhow!("find scrimlet IP: {e}"))?;

    let deadline = Instant::now() + Duration::from_secs(600);
    let mut last = String::new();

    // Attempt to connect until success, then reuse the tunnel.
    let mut conn: Option<(Tunnel, Client)> = None;
    let conn = loop {
        let step = if conn.as_mut().is_none_or(|(t, _)| t.dead()) {
            match connect(&gz_ip, &d.log) {
                Ok(c) => {
                    conn = Some(c);
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    "tunnel opened".to_string()
                }
                Err(e) => {
                    conn = None;
                    format!("{e:#}")
                }
            }
        } else {
            match conn.as_ref().unwrap().1.get_location().await {
                Ok(_) => break conn.take().unwrap(),
                Err(e) => e.to_string(),
            }
        };
        if Instant::now() >= deadline {
            bail!("commission API not up within 600s: {step}");
        }
        if step != last {
            slog::info!(d.log, "{tag}: waiting for the commission API: {step}");
            last = step;
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    };
    slog::info!(d.log, "{tag}: commission API reachable on {scrimlet_name}");
    Ok(conn)
}

/// Discover the switch zone's bootstrap address, open a tunnel to its
/// commission port, and build a client over it.
fn connect(gz_ip: &str, log: &slog::Logger) -> Result<(Tunnel, Client)> {
    let zone_addr =
        zone_bootstrap_addr(gz_ip).context("switch-zone bootstrap address")?;
    let tunnel =
        Tunnel::open(gz_ip, &zone_addr).context("open commission tunnel")?;
    let client = Client::new_with_client(
        &tunnel.base_url(),
        reqwest::Client::builder().build().context("http client")?,
        log.clone(),
    );
    Ok((tunnel, client))
}

/// An ssh local-forward from a host loopback port to the commission API's
/// in-zone loopback, jumping through the scrimlet global zone. Kills the ssh
/// child on drop.
pub(crate) struct Tunnel {
    child: Child,
    local_port: u16,
}

impl Tunnel {
    fn open(gz_ip: &str, zone_addr: &str) -> Result<Self> {
        let local_port = free_local_port()?;
        let askpass = ensure_askpass().context("ssh askpass helper")?;
        let opts: Vec<&str> = EPHEMERAL_HOST_OPTS
            .iter()
            .chain(PASSWORD_AUTH_OPTS)
            .copied()
            .collect();

        // Note, the quoting below is important: even though `Command::arg`
        // doesn't pass args through the shell, ssh will execute the command for
        // ProxyCommand in the local shell, and zsh will try to do a glob with
        // the square brackets (which in turn are needed to allow ssh to
        // properly split the host/port pair on the color character) so this
        // will error out without the single quotes for zsh users:
        let proxy = format!("ssh {} -W '[%h]:%p' root@{gz_ip}", opts.join(" "));
        let child = Command::new("ssh")
            .env("SSH_ASKPASS", &askpass)
            .env("SSH_ASKPASS_REQUIRE", "force")
            .stdin(Stdio::null())
            // Attempts before the zone's sshd amendment lands fail by design
            // and would spam auth denials into the launch log; `dead()` drives
            // the retry, and the caller's deadline reports a real failure.
            .stderr(Stdio::null())
            .args(&opts)
            .arg("-o")
            .arg(format!("ProxyCommand={proxy}"))
            .arg("-N")
            .arg("-L")
            .arg(format!("127.0.0.1:{local_port}:[::1]:{COMMISSION_PORT}"))
            .arg(format!("root@{zone_addr}"))
            .spawn()
            .context("spawn ssh tunnel")?;
        Ok(Self { child, local_port })
    }

    fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.local_port)
    }

    /// Whether the ssh child has exited (auth/connect failed, e.g. the zone
    /// sshd was not up yet), so the tunnel needs rebuilding.
    fn dead(&mut self) -> bool {
        !matches!(self.child.try_wait(), Ok(None))
    }
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A free host loopback port for the tunnel. The window between closing the
/// probe listener and ssh binding is negligible on a lab box.
fn free_local_port() -> Result<u16> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")
        .context("probe a free port")?;
    Ok(listener.local_addr()?.port())
}

/// Where a launched rack's CA certificate is kept, relative to the anchored
/// workdir. It lives in `.falcon/`, so teardown removes it with the rack.
/// Clients such as commtest trust the rack's API through it.
pub(crate) fn rack_ca_path(rack: usize) -> Utf8PathBuf {
    Utf8PathBuf::from(format!(".falcon/rack{}-ca.pem", rack + 1))
}

/// Issue the rack's TLS cert + key (PEM) from a throwaway per-rack CA, whose
/// certificate is written to `ca_out`. wicketd requires a pair before it runs
/// RSS and validates it against the silo hostname (`*.sys.<zone>`).
///
/// A CA rather than a self-signed leaf, because rustls rejects a CA
/// certificate presented as the server's own. No IP SANs: Nexus selects its
/// certificate by SNI and refuses handshakes without one, so it is only ever
/// reached by name.
pub(crate) fn gen_cert(
    zone: &str,
    ca_out: &Utf8Path,
) -> Result<(String, String)> {
    let dir = crate::util::temp_dir()
        .join(format!("voxel-commission-{}", std::process::id()));
    std::fs::create_dir_all(&dir).with_context(|| format!("mkdir {dir}"))?;
    let path = |name: &str| dir.join(name);
    let openssl = |args: &[&str], what: &str| -> Result<()> {
        let out = Command::new("openssl")
            .args(args)
            .output()
            .with_context(|| format!("run openssl to {what}"))?;
        if !out.status.success() {
            bail!(
                "openssl {what} failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(())
    };

    let san = format!("DNS:*.sys.{zone},DNS:*.{zone},DNS:{zone}");
    std::fs::write(
        path("leaf.ext"),
        format!(
            "basicConstraints = critical, CA:FALSE\n\
             keyUsage = critical, digitalSignature, keyEncipherment\n\
             extendedKeyUsage = serverAuth\n\
             subjectAltName = {san}\n"
        ),
    )
    .context("write the leaf extensions")?;

    let (ca_key, ca_pem) = (path("ca.key"), path("ca.pem"));
    let (key, csr, cert) =
        (path("key.pem"), path("leaf.csr"), path("cert.pem"));
    openssl(
        &[
            "req",
            "-x509",
            "-newkey",
            "rsa:2048",
            "-nodes",
            "-days",
            "3650",
            "-sha256",
            "-subj",
            &format!("/CN=voxel {zone} CA"),
            "-keyout",
            ca_key.as_str(),
            "-out",
            ca_pem.as_str(),
        ],
        "generate the rack CA",
    )?;
    openssl(
        &[
            "req",
            "-new",
            "-newkey",
            "rsa:2048",
            "-nodes",
            "-subj",
            &format!("/CN=*.sys.{zone}"),
            "-keyout",
            key.as_str(),
            "-out",
            csr.as_str(),
        ],
        "generate the rack key",
    )?;
    openssl(
        &[
            "x509",
            "-req",
            "-in",
            csr.as_str(),
            "-CA",
            ca_pem.as_str(),
            "-CAkey",
            ca_key.as_str(),
            "-set_serial",
            "1",
            // The most Apple's verifier accepts for a server certificate,
            // even under a private CA. A lab rack never lives that long.
            "-days",
            "397",
            "-sha256",
            "-extfile",
            path("leaf.ext").as_str(),
            "-out",
            cert.as_str(),
        ],
        "sign the rack certificate",
    )?;

    if let Some(parent) = ca_out.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("mkdir {parent}"))?;
    }
    std::fs::copy(&ca_pem, ca_out)
        .with_context(|| format!("save the rack CA to {ca_out}"))?;
    let pair =
        (std::fs::read_to_string(&cert)?, std::fs::read_to_string(&key)?);
    let _ = std::fs::remove_dir_all(&dir);
    Ok(pair)
}

/// Hidden `voxel commission-dryrun`: print the typed rack-setup config body
/// for `rack` as JSON.
pub(crate) fn dryrun(cfg: &VoxelConfig, rack: usize) -> Result<()> {
    let body = rss_config(cfg, rack)?;
    println!("{}", serde_json::to_string_pretty(&body)?);
    Ok(())
}

/// An IpRange from first/last strings of the same address family.
fn ip_range(first: &str, last: &str) -> Result<types::IpRange> {
    let f: IpAddr = first.parse().context("pool first")?;
    let l: IpAddr = last.parse().context("pool last")?;
    match (f, l) {
        (IpAddr::V4(a), IpAddr::V4(b)) => Ok(types::IpRange::V4(
            types::Ipv4Range::new(a, b).map_err(|e| anyhow!("pool: {e}"))?,
        )),
        (IpAddr::V6(a), IpAddr::V6(b)) => Ok(types::IpRange::V6(
            types::Ipv6Range::new(a, b).map_err(|e| anyhow!("pool: {e}"))?,
        )),
        _ => bail!("pool first/last must be the same address family"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(network: &str) -> VoxelConfig {
        VoxelConfig::from_toml(&format!(
            "[topology]\nsleds = 3\n[network]\n{network}"
        ))
        .unwrap()
    }

    #[test]
    fn bgp_config_has_unnumbered_peers_and_addrconf() {
        let cfg = config("router_mode = \"bgp\"");
        let c = rss_config(&cfg, 0).unwrap();
        assert_eq!(
            c.bootstrap_sleds,
            BTreeSet::from([0u16, 1, 2]),
            "all three sleds are bootstrap slots"
        );
        assert!(!c.rack_network_config.switch0.is_empty());
        assert!(c.external_jumbo_frames_opt_in_enabled);
        let types::UserSpecifiedPortConfig::Uplink(port) = c
            .rack_network_config
            .switch0
            .values()
            .next()
            .expect("switch0 has a port")
        else {
            panic!("expected an uplink port config");
        };
        assert_eq!(port.bgp_peers.len(), 1);
        assert!(matches!(
            port.bgp_peers[0].addr,
            types::UserSpecifiedRouterPeerAddr::Unnumbered
        ));
        assert_eq!(c.rack_network_config.bgp.len(), 1);
    }

    #[test]
    fn static_config_has_numbered_uplinks_and_no_bgp() {
        let cfg = config("router_mode = \"static\"");
        let c = rss_config(&cfg, 0).unwrap();
        assert!(c.rack_network_config.bgp.is_empty());
        let types::UserSpecifiedPortConfig::Uplink(port) = c
            .rack_network_config
            .switch0
            .values()
            .next()
            .expect("switch0 has a port")
        else {
            panic!("expected an uplink port config");
        };
        assert!(port.bgp_peers.is_empty());
        assert_eq!(port.routes.len(), 1);
    }
}
