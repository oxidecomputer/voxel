// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Node access: commands, serial, and ssh into sled global zones and switch
//! zones.

use anyhow::{Context, bail};
use libfalcon::{NodeRef, cli::console};
use voxel_config::{SledDesc, VoxelConfig};

use crate::net::{ZLOGIN, resolve_external_ip, ssh_output, zlogin};
use crate::topo::{Topo, build_topo};

/// Run a command in a sled's global zone over ssh and print its output.
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
        .with_context(|| {
            format!("is the rack up? (`voxel serial {sled}` for the console)")
        })?;
    let out = ssh_output(&ip, command).with_context(|| {
        format!("couldn't ssh root@{ip} ({sled}) - is the rack up?")
    })?;
    print!("{out}");
    Ok(())
}

/// Run a command inside a switch zone over ssh and zlogin and print its output.
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
        format!("is the rack up? (`voxel serial {}` for the console)", s.name)
    })?;
    let out = ssh_output(&ip, &zlogin(command)).with_context(|| {
        format!("couldn't reach oxz_switch on {} ({switch})", s.name)
    })?;
    print!("{out}");
    Ok(())
}

pub(crate) async fn cmd_serial(
    cfg: &VoxelConfig,
    name: &str,
    node: &str,
) -> anyhow::Result<()> {
    let topo = build_topo(cfg, name)?;
    if topo.node_ref(node).is_none() {
        bail!("no such node: {node}");
    }
    let dir = topo.runner.get_falcon_dir();
    console(node, camino::Utf8Path::new(&dir)).await.context("serial")
}

/// Hand the terminal to ssh as root at ip, replacing this process. An optional
/// remote command runs instead of a shell.
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
    // exec returns only when ssh failed to launch.
    bail!("could not exec ssh: {}", c.exec())
}

pub(crate) async fn cmd_host_ls(
    cfg: &VoxelConfig,
    name: &str,
) -> anyhow::Result<()> {
    let topo = build_topo(cfg, name)?;
    println!(
        "{:<6}  {:<16}  {:<8}  {:<8}  {:<10}  SP",
        "NODE", "IP", "ROLE", "PID", "PROPOLIS"
    );
    let mut rows: Vec<(String, NodeRef, &str, Option<usize>)> = topo
        .sleds
        .iter()
        .map(|(s, n)| {
            let role = if s.scrimlet { "scrimlet" } else { "gimlet" };
            (s.name.clone(), *n, role, Some(s.rack))
        })
        .collect();
    rows.extend(
        topo.routers.iter().map(|(r, n)| (r.clone(), *n, "router", None)),
    );
    for (node, n, role, rack) in rows {
        let is_router = role == "router";
        let pid =
            crate::node::read_pidfile(&node).unwrap_or_else(|| "-".into());
        let state = crate::node::propolis_state(&node).await;
        // A powered off node has no console to ask. Do not wait on it.
        let ip = if state == "down" {
            "-".to_string()
        } else {
            resolve_external_ip(cfg, &topo.runner, &node, n, is_router)
                .await
                .unwrap_or_else(|_| "(unknown)".into())
        };
        // The SP's host power, where an emulated fleet serves this sled.
        let sp = rack
            .and_then(|rack| crate::power::sp_state(cfg, rack, &node))
            .unwrap_or_else(|| "-".into());
        println!("{node:<6}  {ip:<16}  {role:<8}  {pid:<8}  {state:<10}  {sp}");
    }
    Ok(())
}

pub(crate) async fn cmd_host_login(
    cfg: &VoxelConfig,
    name: &str,
    node: &str,
) -> anyhow::Result<()> {
    let topo = build_topo(cfg, name)?;
    // Routers accept the same root ssh login; the FRR image bakes it in.
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
        .with_context(|| {
        format!("is the rack up? (`voxel serial {node}` for the console)")
    })?;
    eprintln!(
        "[voxel] ssh root@{ip}  ({node} {})",
        if is_router { "router" } else { "global zone" }
    );
    ssh_exec(&ip, None)
}

/// Resolve a switch argument to its scrimlet.
pub(crate) fn resolve_switch<'a>(
    topo: &'a Topo,
    switch: &str,
) -> anyhow::Result<&'a (SledDesc, NodeRef)> {
    let scrimlets: Vec<&(SledDesc, NodeRef)> =
        topo.sleds.iter().filter(|(s, _)| s.scrimlet).collect();

    // Node name, always unambiguous.
    if let Some(hit) = scrimlets.iter().find(|(s, _)| s.name == switch) {
        return Ok(hit);
    }
    // Rack qualified rackR/switchS, R 1-based.
    if let Some((r, sw)) = switch.split_once('/')
        && let (Some(rack), Some(slot)) = (
            r.strip_prefix("rack").and_then(|x| x.parse::<usize>().ok()),
            sw.strip_prefix("switch").and_then(|x| x.parse::<usize>().ok()),
        )
    {
        let rack0 = rack.saturating_sub(1);
        let hit = scrimlets
            .iter()
            .filter(|(s, _)| s.rack == rack0)
            .nth(slot)
            .with_context(|| {
            format!("no rack{rack}/switch{slot} in topology")
        })?;
        return Ok(hit);
    }
    // Bare switchN: the global Nth scrimlet.
    if let Some(n) =
        switch.strip_prefix("switch").and_then(|x| x.parse::<usize>().ok())
    {
        return scrimlets
            .into_iter()
            .nth(n)
            .with_context(|| format!("no scrimlet for {switch}"));
    }
    bail!(
        "unknown switch '{switch}' (expected <scrimlet>|switchN|rackR/switchS)"
    )
}

pub(crate) async fn cmd_tp_ls(
    cfg: &VoxelConfig,
    name: &str,
) -> anyhow::Result<()> {
    let topo = build_topo(cfg, name)?;
    // Each rack has its own switch0 and switch1. Number the slot per rack and
    // show the rack, 1-based as in voxel info.
    let multi = cfg.topology.racks() > 1;
    if multi {
        println!("{:<6}  {:<8}  {:<6}  IP", "RACK", "SWITCH", "NODE");
    } else {
        println!("{:<8}  {:<6}  IP", "SWITCH", "NODE");
    }
    let mut per_rack: std::collections::BTreeMap<usize, usize> =
        std::collections::BTreeMap::new();
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
        format!("is the rack up? (`voxel serial {}` for the console)", s.name)
    })?;
    eprintln!("[voxel] ssh root@{ip} -> {ZLOGIN}  ({} {switch})", s.name);
    ssh_exec(&ip, Some(ZLOGIN))
}
