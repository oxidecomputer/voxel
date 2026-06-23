//! Drive rack setup THROUGH wicketd -- the real operator flow -- instead of the
//! file-based sled-agent auto-init. With this path wicket's RACK SETUP page is
//! fully populated (NTP/DNS/pools/uplinks/certs/recovery password) and init is
//! genuinely wicketd-driven (it assembles the `RackInitializeRequest` from the
//! uploaded config + the SPs it discovered). Gated on `launch --wicket-setup`.
//!
//! Sequence, once the switch zone's wicketd is up and MGS has discovered the SPs
//! (so wicketd's `bootstrap_sleds` is populated -- our baseboard fix is what makes
//! that correlate):
//!   PUT  /rack-setup/config                            (ntp/dns/pools/network/…)
//!   POST /rack-setup/config/cert  +  /key              (a self-signed TLS pair)
//!   PUT  /rack-setup/config/recovery-user-password-hash
//!   POST /rack-setup                                   (trigger init)
//! then the normal `watch_rss` reports the wicketd-triggered bring-up.
//!
//! The config body is reshaped from the same `config-rss.toml` voxel-rss-gen
//! produces (release-accurate by construction) into wicket's `UserSpecified*`
//! form -- the one fiddly bit is the flat-string serializations
//! (`address = "addrconf"`, `addr = "unnumbered"`), validated against live wicketd.

use crate::net::{node_external_ip, scp_to, ssh_capture};
use anyhow::{anyhow, Context, Result};
use libfalcon::{NodeRef, Runner};
use slog::info;
use std::path::Path;
use std::time::{Duration, Instant};

/// wicketd's dropshot address inside oxz_switch (loopback only).
const WICKETD: &str = "http://[::1]:12226";

/// Offline check (hidden `voxel wicket-dryrun`): parse a config-rss.toml and
/// print the wicketd `PutRssUserConfigInsensitive` body it would PUT, so the
/// reshape can be validated against live wicketd without a relaunch.
pub(crate) fn dryrun(config_rss_path: &Path, num_sleds: usize) -> Result<()> {
    let config_rss = std::fs::read_to_string(config_rss_path)
        .with_context(|| format!("read {}", config_rss_path.display()))?;
    // Offline check assumes a single rack (slots 0..n); the live multi-rack slot
    // set comes from the topology in `drive`.
    let slots: Vec<u16> = (0..num_sleds as u16).collect();
    let (body, pw_hash) = build_bodies(&config_rss, &slots)?;
    println!("{body}");
    eprintln!("[dryrun] recovery password hash present: {}", !pw_hash.is_empty());
    Ok(())
}

/// Reshape `config-rss.toml` into wicketd's `PutRssUserConfigInsensitive` JSON,
/// and pull out the recovery-user password hash. `bootstrap_slots` are the cubby
/// slot numbers wicketd maps to discovered SPs - for rack R these are that rack's
/// gimlet GLOBAL indices (rack 1's sleds sit in cubbies 3,4,5, not 0,1,2), since
/// the MGS sim reports each gimlet at `location = ["sled", global_index]`. Passing
/// a wrong/`0..n` set leaves wicketd unable to correlate rack 1's sleds and it
/// never initializes.
fn build_bodies(config_rss: &str, bootstrap_slots: &[u16]) -> Result<(String, String)> {
    let v: toml::Value = toml::from_str(config_rss).context("parse config-rss.toml")?;
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
            let switch = p.get("switch").and_then(|s| s.as_str()).unwrap_or("switch0");
            let port = p.get("port").and_then(|s| s.as_str()).unwrap_or("qsfp0").to_string();
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
        "internal_services_ip_pool_ranges": arr("internal_services_ip_pool_ranges"),
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
        .ok_or_else(|| anyhow!("config-rss has no recovery_silo.user_password_hash"))?
        .to_string();

    Ok((serde_json::to_string(&body)?, pw_hash))
}

/// Reduce one config-rss port table to wicket's `UserSpecifiedPortConfig`,
/// flattening the address + bgp-peer addr to their string forms.
fn reshape_port(p: &toml::Value) -> serde_json::Value {
    let addresses: Vec<serde_json::Value> = p
        .get("addresses")
        .and_then(|a| a.as_array())
        .map(|arr| arr.iter().map(|a| serde_json::json!({"address": flat_uplink_addr(a)})).collect())
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
        toml::Value::Array(a) => serde_json::Value::Array(a.into_iter().map(toml_to_json).collect()),
        toml::Value::Table(t) => {
            serde_json::Value::Object(t.into_iter().map(|(k, v)| (k, toml_to_json(v))).collect())
        }
    }
}

/// Generate a self-signed TLS cert + key (PEM) for the recovery silo under
/// `zone`, via `openssl` on the box. wicketd *requires* a cert pair before it'll
/// run RSS, and validates it against the silo's external hostname
/// (`*.sys.<zone>` covers `recovery.sys.<zone>`).
fn gen_cert(zone: &str) -> Result<(String, String)> {
    let dir = std::env::temp_dir();
    let key = dir.join("voxel-wicket-key.pem");
    let cert = dir.join("voxel-wicket-cert.pem");
    let san = format!("DNS:*.sys.{zone},DNS:*.{zone},DNS:{zone}");
    let status = std::process::Command::new("openssl")
        .args(["req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "3650", "-sha256"])
        .arg("-keyout").arg(&key)
        .arg("-out").arg(&cert)
        .args(["-subj", &format!("/CN=*.sys.{zone}")])
        .args(["-addext", &format!("subjectAltName={san}")])
        .status()
        .context("run openssl to generate the RSS cert")?;
    if !status.success() {
        return Err(anyhow!("openssl cert generation failed"));
    }
    Ok((std::fs::read_to_string(&cert)?, std::fs::read_to_string(&key)?))
}

/// JSON-encode a string body (`TypedBody<String>` -- the cert/key endpoints take
/// the PEM as a JSON string).
fn json_string(s: &str) -> String {
    serde_json::Value::String(s.to_string()).to_string()
}

/// Ship `body` into the switch zone and `curl` it to wicketd with `method`,
/// returning the HTTP status code. Bodies go via a file (`--data @`) to dodge
/// shell quoting; `name` is the in-zone temp filename.
fn wicketd_call(ip: &str, method: &str, path: &str, body: &str, name: &str) -> Result<u32> {
    let local = std::env::temp_dir().join(name);
    std::fs::write(&local, body).with_context(|| format!("write {name}"))?;
    let zone_path = format!("/zone/oxz_switch/root/var/tmp/{name}");
    if !scp_to(ip, local.to_str().unwrap(), &zone_path) {
        return Err(anyhow!("scp {name} to {ip} switch zone failed"));
    }
    let curl = format!(
        "zlogin oxz_switch curl -s -o /dev/null -w '%{{http_code}}' -X {method} \
         -H content-type:application/json --data @/var/tmp/{name} {WICKETD}{path}"
    );
    let code = ssh_capture(ip, &curl).ok_or_else(|| anyhow!("ssh curl {path} failed"))?;
    code.trim().parse::<u32>().map_err(|_| anyhow!("{path}: unexpected response {code:?}"))
}

/// Poll wicketd until it's up and has discovered the rack's SPs (so
/// `bootstrap_sleds` is populated and a PUT will be accepted).
fn wait_wicketd_ready(ip: &str, num_sleds: usize, log: &slog::Logger) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(300);
    let curl = format!("zlogin oxz_switch curl -s {WICKETD}/bootstrap-sleds");
    loop {
        if let Some(out) = ssh_capture(ip, &curl) {
            let found = out.matches("\"identifier\"").count();
            if found >= num_sleds {
                info!(log, "wicket-setup: wicketd ready, {found} sleds discovered");
                return Ok(());
            }
        }
        if Instant::now() >= deadline {
            return Err(anyhow!("wicketd not ready / SPs not discovered within 5m"));
        }
        std::thread::sleep(Duration::from_secs(8));
    }
}

/// Drive the full wicketd setup for one rack, then return so the caller can
/// `watch_rss` the wicketd-triggered bring-up.
pub(crate) async fn drive(
    d: &Runner,
    scrimlet: NodeRef,
    bootstrap_slots: &[u16],
    config_rss_path: &Path,
    zone: &str,
    tag: &str,
) -> Result<()> {
    info!(d.log, "{tag}: driving rack setup through wicketd");
    let ip = node_external_ip(d, scrimlet, false)
        .await
        .map_err(|e| anyhow!("find switch-zone scrimlet IP: {e}"))?;
    wait_wicketd_ready(&ip, bootstrap_slots.len(), &d.log)?;

    let config_rss = std::fs::read_to_string(config_rss_path)
        .with_context(|| format!("read {}", config_rss_path.display()))?;
    let (config_body, pw_hash) = build_bodies(&config_rss, bootstrap_slots)?;
    let (cert_pem, key_pem) = gen_cert(zone)?;

    // Upload the config, the cert/key pair, and the recovery password.
    // Right after wicketd reports the SPs discovered (`wait_wicketd_ready`) it can
    // still transiently reject the config PUT with HTTP 400 for a few seconds while
    // its internal SP inventory settles - observed on the SECOND rack of a
    // multi-rack launch, which comes up under the first (already-initialized)
    // rack's full load. The body is valid (a re-PUT moments later returns 204), so
    // retry a handful of times before giving up.
    let mut put_config = 0;
    for attempt in 1..=10 {
        put_config = wicketd_call(&ip, "PUT", "/rack-setup/config", &config_body, "wsetup-config.json")?;
        if put_config == 204 {
            break;
        }
        info!(d.log, "{tag}: PUT /rack-setup/config -> HTTP {put_config} (attempt {attempt}/10); wicketd still settling, retrying in 6s");
        std::thread::sleep(Duration::from_secs(6));
    }
    if put_config != 204 {
        return Err(anyhow!("PUT /rack-setup/config -> HTTP {put_config} after 10 attempts"));
    }
    let c = wicketd_call(&ip, "POST", "/rack-setup/config/cert", &json_string(&cert_pem), "wsetup-cert.json")?;
    let k = wicketd_call(&ip, "POST", "/rack-setup/config/key", &json_string(&key_pem), "wsetup-key.json")?;
    if !(200..300).contains(&c) || !(200..300).contains(&k) {
        return Err(anyhow!("cert/key upload -> HTTP {c}/{k}"));
    }
    let pw = wicketd_call(
        &ip,
        "PUT",
        "/rack-setup/config/recovery-user-password-hash",
        &format!("{{\"hash\":{}}}", json_string(&pw_hash)),
        "wsetup-pw.json",
    )?;
    if pw != 204 {
        return Err(anyhow!("PUT recovery-user-password-hash -> HTTP {pw}"));
    }
    info!(d.log, "{tag}: config + cert + recovery password uploaded; triggering RSS");

    // Trigger init. `post_run_rack_setup` assembles the RackInitializeRequest from
    // the uploaded config + the discovered bootstrap_sleds and calls the
    // bootstrap-agent -- after this the normal /rack-initialize status reflects it.
    let run = wicketd_call(&ip, "POST", "/rack-setup", "{}", "wsetup-run.json")?;
    if !(200..300).contains(&run) {
        return Err(anyhow!("POST /rack-setup -> HTTP {run}"));
    }
    info!(d.log, "{tag}: wicketd-driven RSS started");
    Ok(())
}
