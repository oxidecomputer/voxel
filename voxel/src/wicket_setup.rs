// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Offline reshape of a config-rss.toml into wicketd's rack-setup JSON body
//! (`voxel wicket-dryrun`); rack setup itself is driven by `crate::commission`.

use anyhow::{Context, Result, anyhow};
use camino::Utf8Path;

/// Offline check (hidden `voxel wicket-dryrun`): parse a config-rss.toml and
/// print the wicketd `PutRssUserConfigInsensitive` body it would PUT, so the
/// reshape can be validated against live wicketd without a relaunch.
pub(crate) fn dryrun(
    config_rss_path: &Utf8Path,
    num_sleds: usize,
) -> Result<()> {
    let config_rss = std::fs::read_to_string(config_rss_path)
        .with_context(|| format!("read {}", config_rss_path))?;
    // Offline check assumes a single rack (slots 0..n); the live multi-rack slot
    // set comes from the topology in `drive`.
    let slots: Vec<u16> = (0..num_sleds as u16).collect();
    let (body, pw_hash) = build_bodies(&config_rss, &slots)?;
    println!("{body}");
    eprintln!(
        "[dryrun] recovery password hash present: {}",
        !pw_hash.is_empty()
    );
    Ok(())
}

/// Reshape `config-rss.toml` into wicketd's `PutRssUserConfigInsensitive` JSON,
/// and pull out the recovery-user password hash. `bootstrap_slots` are the cubby
/// slot numbers wicketd maps to discovered SPs - for rack R these are that rack's
/// gimlet GLOBAL indices (rack 1's sleds sit in cubbies 3,4,5, not 0,1,2), since
/// the MGS sim reports each gimlet at `location = ["sled", global_index]`. Passing
/// a wrong/`0..n` set leaves wicketd unable to correlate rack 1's sleds and it
/// never initializes.
fn build_bodies(
    config_rss: &str,
    bootstrap_slots: &[u16],
) -> Result<(String, String)> {
    let v: toml::Value =
        toml::from_str(config_rss).context("parse config-rss.toml")?;
    let arr = |k: &str| -> serde_json::Value {
        toml_to_json(v.get(k).cloned().unwrap_or(toml::Value::Array(vec![])))
    };
    let rnc = v
        .get("rack_network_config")
        .ok_or_else(|| anyhow!("config-rss has no rack_network_config"))?;

    // Reshape the network config: the flat `ports = [{switch,port,…}]` array
    // becomes per-switch maps keyed by port name, each address/peer reduced to the
    // flat-string form wicket wants. `bgp` and infra IPs pass through.
    let mut switch0 = serde_json::Map::new();
    let mut switch1 = serde_json::Map::new();
    if let Some(ports) = rnc.get("ports").and_then(|p| p.as_array()) {
        for p in ports {
            let switch =
                p.get("switch").and_then(|s| s.as_str()).unwrap_or("switch0");
            let port = p
                .get("port")
                .and_then(|s| s.as_str())
                .unwrap_or("qsfp0")
                .to_string();
            let cfg = reshape_port(p);
            if switch == "switch1" {
                switch1.insert(port, cfg);
            } else {
                switch0.insert(port, cfg);
            }
        }
    }

    let rack_network_config = serde_json::json!({
        // wicket picks the rack subnet itself; leave it unset.
        "rack_subnet_address": serde_json::Value::Null,
        "infra_ip_first": rnc.get("infra_ip_first").and_then(|x| x.as_str()).unwrap_or("::"),
        "infra_ip_last": rnc.get("infra_ip_last").and_then(|x| x.as_str()).unwrap_or("::"),
        "switch0": serde_json::Value::Object(switch0),
        "switch1": serde_json::Value::Object(switch1),
        "bgp": toml_to_json(rnc.get("bgp").cloned().unwrap_or(toml::Value::Array(vec![]))),
    });

    let allowed_source_ips = v
        .get("allowed_source_ips")
        .map(|a| toml_to_json(a.clone()))
        .unwrap_or_else(|| serde_json::json!({"allow": "any"}));

    let body = serde_json::json!({
        "bootstrap_sleds": bootstrap_slots,
        "ntp_servers": arr("ntp_servers"),
        "dns_servers": arr("dns_servers"),
        "service_ip_pools": arr("service_ip_pools"),
        "external_dns_ips": arr("external_dns_ips"),
        "external_dns_zone_name": v.get("external_dns_zone_name").and_then(|x| x.as_str()).unwrap_or(""),
        // Enable the fleet-wide jumbo-frames opt-in for voxel racks set up via
        // wicketd (the file path leaves it off; operator default here is on).
        "external_jumbo_frames_opt_in_enabled": true,
        "allowed_source_ips": allowed_source_ips,
        "rack_network_config": rack_network_config,
    });

    let pw_hash = v
        .get("recovery_silo")
        .and_then(|r| r.get("user_password_hash"))
        .and_then(|h| h.as_str())
        .ok_or_else(|| {
            anyhow!("config-rss has no recovery_silo.user_password_hash")
        })?
        .to_string();

    Ok((serde_json::to_string(&body)?, pw_hash))
}

/// Reduce one config-rss port table to wicket's `UserSpecifiedPortConfig`,
/// flattening the address + bgp-peer addr to their string forms.
fn reshape_port(p: &toml::Value) -> serde_json::Value {
    let addresses: Vec<serde_json::Value> = p
        .get("addresses")
        .and_then(|a| a.as_array())
        .map(|arr| {
            arr.iter()
                .map(|a| serde_json::json!({"address": flat_uplink_addr(a)}))
                .collect()
        })
        .unwrap_or_default();
    let bgp_peers: Vec<serde_json::Value> = p
        .get("bgp_peers")
        .and_then(|a| a.as_array())
        .map(|arr| {
            arr.iter()
                .map(|peer| {
                    serde_json::json!({
                        "asn": peer.get("asn").and_then(|x| x.as_integer()).unwrap_or(0),
                        "port": peer.get("port").and_then(|x| x.as_str()).unwrap_or("qsfp0"),
                        "addr": flat_peer_addr(peer.get("addr")),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    serde_json::json!({
        "routes": toml_to_json(p.get("routes").cloned().unwrap_or(toml::Value::Array(vec![]))),
        "addresses": addresses,
        "uplink_port_speed": p.get("uplink_port_speed").and_then(|x| x.as_str()).unwrap_or("speed40_g"),
        "uplink_port_fec": p.get("uplink_port_fec").and_then(|x| x.as_str()).unwrap_or("none"),
        "autoneg": p.get("autoneg").and_then(|x| x.as_bool()).unwrap_or(false),
        "bgp_peers": bgp_peers,
        "lldp": p.get("lldp").map(|l| toml_to_json(l.clone())).unwrap_or(serde_json::Value::Null),
    })
}

/// config-rss `address = {type="addrconf"}` -> "addrconf"; `{type="static", ip_net=…}` -> the cidr.
fn flat_uplink_addr(a: &toml::Value) -> String {
    let addr = a.get("address").unwrap_or(a);
    match addr.get("type").and_then(|t| t.as_str()) {
        Some("static") => addr
            .get("ip_net")
            .and_then(|n| n.as_str())
            .unwrap_or("addrconf")
            .to_string(),
        _ => "addrconf".to_string(),
    }
}

/// config-rss peer `addr = {type="unnumbered", …}` -> "unnumbered"; numbered -> the IP.
fn flat_peer_addr(addr: Option<&toml::Value>) -> String {
    match addr.and_then(|a| a.get("type")).and_then(|t| t.as_str()) {
        Some("unnumbered") | None => "unnumbered".to_string(),
        Some(_) => addr
            .and_then(|a| a.get("value").or_else(|| a.get("addr")))
            .and_then(|x| x.as_str())
            .unwrap_or("unnumbered")
            .to_string(),
    }
}

/// Minimal TOML->JSON value conversion (scalars, arrays, tables).
fn toml_to_json(v: toml::Value) -> serde_json::Value {
    match v {
        toml::Value::String(s) => serde_json::Value::String(s),
        toml::Value::Integer(i) => serde_json::Value::Number(i.into()),
        toml::Value::Boolean(b) => serde_json::Value::Bool(b),
        toml::Value::Float(f) => serde_json::json!(f),
        toml::Value::Datetime(d) => serde_json::Value::String(d.to_string()),
        toml::Value::Array(a) => {
            serde_json::Value::Array(a.into_iter().map(toml_to_json).collect())
        }
        toml::Value::Table(t) => serde_json::Value::Object(
            t.into_iter().map(|(k, v)| (k, toml_to_json(v))).collect(),
        ),
    }
}
