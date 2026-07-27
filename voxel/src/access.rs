//! Node access: run commands, attach serial, and pilot-style SSH into sled
//! global zones (`host`) and switch zones (`tp`).

use anyhow::{Context, bail};
use libfalcon::{NodeRef, cli::console};
use voxel_config::{SledDesc, VoxelConfig};

use crate::net::{ZLOGIN, resolve_external_ip, ssh_output, zlogin};
use crate::topo::{Topo, build_topo};

/// `voxel host exec -c "<cmd>" <sled>` - run a command in a sled's global zone
/// over ssh (the non-interactive `host login`) and print its output.
pub(crate) async fn cmd_host_exec(
    cfg: &VoxelConfig,
    name: &str,
    sled: &str,
    command: &str,
) -> anyhow::Result<()> {
    let topo = build_topo(cfg, name)?;
    let (_, n) = topo
        .sleds
        .iter()
        .find(|(s, _)| s.name == sled)
        .with_context(|| format!("no such sled: {sled}"))?;
    let ip = resolve_external_ip(cfg, &topo.runner, sled, *n, false)
        .await
        .with_context(|| format!("is the rack up? (`voxel serial {sled}` for the console)"))?;
    let out = ssh_output(&ip, command)
        .with_context(|| format!("couldn't ssh root@{ip} ({sled}) - is the rack up?"))?;
    print!("{out}");
    Ok(())
}

/// `voxel tp exec -c "<cmd>" <switch>` - run a command inside a switch zone
/// (`oxz_switch`, where swadm/dpd/mgadm live) over ssh+zlogin and print its
/// output. The non-interactive `tp login`.
pub(crate) async fn cmd_tp_exec(
    cfg: &VoxelConfig,
    name: &str,
    switch: &str,
    command: &str,
) -> anyhow::Result<()> {
    let topo = build_topo(cfg, name)?;
    let (s, n) = resolve_switch(&topo, switch)?;
    let ip = resolve_external_ip(cfg, &topo.runner, &s.name, *n, false)
        .await
        .with_context(|| {
            format!(
                "is the rack up? (`voxel serial {}` for the console)",
                s.name
            )
        })?;
    let out = ssh_output(&ip, &zlogin(command))
        .with_context(|| format!("couldn't reach oxz_switch on {} ({switch})", s.name))?;
    print!("{out}");
    Ok(())
}

pub(crate) async fn cmd_serial(cfg: &VoxelConfig, name: &str, node: &str) -> anyhow::Result<()> {
    let topo = build_topo(cfg, name)?;
    if topo.node_ref(node).is_none() {
        bail!("no such node: {node}");
    }
    let dir = topo.runner.get_falcon_dir();
    console(node, camino::Utf8Path::new(&dir))
        .await
        .context("serial")
}

/// Hand the terminal to `ssh root@<ip>` (optionally running a remote command),
/// replacing this process - the pilot/captain access pattern.
fn ssh_exec(ip: &str, remote: Option<&str>) -> anyhow::Result<()> {
    use std::os::unix::process::CommandExt;
    let mut c = std::process::Command::new("ssh");
    c.args(crate::net::EPHEMERAL_HOST_OPTS);
    if remote.is_some() {
        c.arg("-t"); // allocate a TTY for the remote interactive command
    }
    c.arg(format!("root@{ip}"));
    if let Some(r) = remote {
        c.arg(r);
    }
    // exec() only returns if it failed to launch ssh.
    bail!("could not exec ssh: {}", c.exec())
}

pub(crate) async fn cmd_host_ls(cfg: &VoxelConfig, name: &str) -> anyhow::Result<()> {
    let topo = build_topo(cfg, name)?;
    println!("{:<6}  {:<16}  {}", "NODE", "IP", "ROLE");
    for (s, n) in &topo.sleds {
        let ip = resolve_external_ip(cfg, &topo.runner, &s.name, *n, false)
            .await
            .unwrap_or_else(|_| "(unknown)".into());
        let role = if s.scrimlet { "scrimlet" } else { "gimlet" };
        println!("{:<6}  {:<16}  {role}", s.name, ip);
    }
    Ok(())
}

pub(crate) async fn cmd_host_login(
    cfg: &VoxelConfig,
    name: &str,
    node: &str,
) -> anyhow::Result<()> {
    let topo = build_topo(cfg, name)?;
    // Routers accept the same root SSH login (the FRR image bakes in sshd with
    // the operator key), so `host login` covers them too.
    let (n, is_router) = topo
        .sleds
        .iter()
        .find(|(s, _)| s.name == node)
        .map(|(_, n)| (*n, false))
        .or_else(|| {
            topo.routers
                .iter()
                .find(|(r, _)| r == node)
                .map(|(_, n)| (*n, true))
        })
        .with_context(|| format!("no such node: {node}"))?;
    let ip = resolve_external_ip(cfg, &topo.runner, node, n, is_router)
        .await
        .with_context(|| format!("is the rack up? (`voxel serial {node}` for the console)"))?;
    eprintln!(
        "[voxel] ssh root@{ip}  ({node} {})",
        if is_router { "router" } else { "global zone" }
    );
    ssh_exec(&ip, None)
}

/// Resolve a switch argument to its scrimlet `(SledDesc, NodeRef)`. Accepts:
/// a scrimlet **node name** (`g3`, always unambiguous); a **rack-qualified**
/// switch `rackR/switchS` (R is 1-based, matching `tp ls` - the right form for a
/// multi-rack deployment where each rack has its own switch0/switch1); or a bare
/// **global** `switchN` (back-compat, the Nth scrimlet - fine for a single rack).
pub(crate) fn resolve_switch<'a>(
    topo: &'a Topo,
    switch: &str,
) -> anyhow::Result<&'a (SledDesc, NodeRef)> {
    let scrimlets: Vec<&(SledDesc, NodeRef)> =
        topo.sleds.iter().filter(|(s, _)| s.scrimlet).collect();

    // Node name - always unambiguous.
    if let Some(hit) = scrimlets.iter().find(|(s, _)| s.name == switch) {
        return Ok(hit);
    }
    // Rack-qualified `rackR/switchS` (R 1-based).
    if let Some((r, sw)) = switch.split_once('/') {
        if let (Some(rack), Some(slot)) = (
            r.strip_prefix("rack").and_then(|x| x.parse::<usize>().ok()),
            sw.strip_prefix("switch")
                .and_then(|x| x.parse::<usize>().ok()),
        ) {
            let rack0 = rack.saturating_sub(1);
            let hit = scrimlets
                .iter()
                .filter(|(s, _)| s.rack == rack0)
                .nth(slot)
                .with_context(|| format!("no rack{rack}/switch{slot} in topology"))?;
            return Ok(hit);
        }
    }
    // Bare `switchN` - global Nth scrimlet.
    if let Some(n) = switch
        .strip_prefix("switch")
        .and_then(|x| x.parse::<usize>().ok())
    {
        return scrimlets
            .into_iter()
            .nth(n)
            .with_context(|| format!("no scrimlet for {switch}"));
    }
    bail!("unknown switch '{switch}' (expected <scrimlet>|switchN|rackR/switchS)")
}

pub(crate) async fn cmd_tp_ls(cfg: &VoxelConfig, name: &str) -> anyhow::Result<()> {
    let topo = build_topo(cfg, name)?;
    // Each rack has its own switch0/switch1, so number the switch slot PER RACK
    // and show which rack it's in (1-based, matching `voxel info`).
    let multi = cfg.topology.racks() > 1;
    if multi {
        println!("{:<6}  {:<8}  {:<6}  {}", "RACK", "SWITCH", "NODE", "IP");
    } else {
        println!("{:<8}  {:<6}  {}", "SWITCH", "NODE", "IP");
    }
    let mut per_rack: std::collections::BTreeMap<usize, usize> = std::collections::BTreeMap::new();
    for (s, n) in topo.sleds.iter().filter(|(s, _)| s.scrimlet) {
        let slot = per_rack.entry(s.rack).or_insert(0);
        let ip = resolve_external_ip(cfg, &topo.runner, &s.name, *n, false)
            .await
            .unwrap_or_else(|_| "(unknown)".into());
        if multi {
            println!(
                "{:<6}  {:<8}  {:<6}  {ip}",
                format!("rack{}", s.rack + 1),
                format!("switch{slot}"),
                s.name
            );
        } else {
            println!("{:<8}  {:<6}  {ip}", format!("switch{slot}"), s.name);
        }
        *slot += 1;
    }
    Ok(())
}

pub(crate) async fn cmd_tp_login(
    cfg: &VoxelConfig,
    name: &str,
    switch: &str,
) -> anyhow::Result<()> {
    let topo = build_topo(cfg, name)?;
    let (s, n) = resolve_switch(&topo, switch)?;
    let ip = resolve_external_ip(cfg, &topo.runner, &s.name, *n, false)
        .await
        .with_context(|| {
            format!(
                "is the rack up? (`voxel serial {}` for the console)",
                s.name
            )
        })?;
    eprintln!("[voxel] ssh root@{ip} -> {ZLOGIN}  ({} {switch})", s.name);
    ssh_exec(&ip, Some(ZLOGIN))
}
