// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The emulated SP fleet on the falcon host: one sp-emu process per board,
//! shared by both switch zones, scoped by rack.

use anyhow::{Context, bail};
use camino::Utf8Path;
use std::process::{Command, Stdio};

/// SMF service backing the fleet, one instance per SP per rack.
const SVC: &str = "svc:/oxide/voxel-sp-emu";
/// Where each rack's manifest is written for import.
const MANIFEST_DIR: &str = "/var/svc/manifest/site";
/// How long to keep retrying a losing import before giving up.
const IMPORT_WAIT_S: u32 = 60;

/// The sp-emu board an SP runs, which selects its staged hubris archive.
fn board_of(sp: &voxel_config::sp::Sp) -> &'static str {
    match sp.role {
        voxel_config::sp::SpRole::Sidecar => "sidecar",
        voxel_config::sp::SpRole::Gimlet(_) => "gimlet",
    }
}

/// A rack's SMF instance name for an SP. It carries the rack index so fleets
/// on the shared host cannot collide.
fn instance(rack: usize, port: u16) -> String {
    format!("r{rack}sp{port}")
}

/// The addrobj holding a rack's fleet address.
fn addrobj(rack: usize) -> String {
    format!("spr{rack}")
}

/// A rack's manifest path.
fn manifest_path(rack: usize) -> String {
    format!("{MANIFEST_DIR}/voxel-sp-emu-r{rack}.xml")
}

/// Run a read-only probe, true when it exits 0.
pub(crate) fn probe(cmd: &str, args: &[&str]) -> bool {
    Command::new(cmd)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Capture a read-only probe's stdout. None on spawn failure or non-zero exit.
pub(crate) fn probe_out(cmd: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(cmd).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Run a mutating host command under pfexec.
pub(crate) fn run(args: &[&str]) -> anyhow::Result<()> {
    let st = Command::new("pfexec")
        .args(args)
        .status()
        .with_context(|| format!("spawn pfexec {}", args.join(" ")))?;
    if !st.success() {
        bail!("pfexec {} failed ({st})", args.join(" "));
    }
    Ok(())
}

/// The SMF manifest for one rack's fleet. Each SP runs in the foreground under
/// startd, binding its MGS pair, its power bridge, and the ignition topology.
fn manifest(
    rack: usize,
    dir: &Utf8Path,
    addr: &str,
    rot: bool,
    fleet: &[&voxel_config::sp::Sp],
) -> String {
    let mut s = format!(
        "<?xml version=\"1.0\"?>\n\
         <!DOCTYPE service_bundle SYSTEM \"/usr/share/lib/xml/dtd/service_bundle.dtd.1\">\n\
         <service_bundle type=\"manifest\" name=\"voxel-sp-emu-r{rack}\">\n\
         <service name=\"oxide/voxel-sp-emu\" type=\"service\" version=\"1\">\n\
         \x20 <dependency name=\"multi_user\" grouping=\"require_all\" restart_on=\"none\" type=\"service\">\n\
         \x20   <service_fmri value=\"svc:/milestone/multi-user:default\"/>\n\
         \x20 </dependency>\n"
    );
    let ignition: Vec<String> =
        fleet.iter().map(|sp| sp.ignition_entry()).collect();
    let ignition = ignition.join(",");
    for sp in fleet {
        let inst = instance(rack, sp.base_port);
        let state = format!("{dir}/state/{}", sp.base_port);
        let board = board_of(sp);
        // working_directory keeps sp-emu's cwd-relative RoT archive extraction
        // inside the instance's state dir.
        s.push_str(&format!(
            "  <instance name=\"{inst}\" enabled=\"true\">\n\
             \x20   <exec_method type=\"method\" name=\"start\" exec=\"{dir}/sp-emu run a 0\" timeout_seconds=\"0\">\n\
             \x20     <method_context working_directory=\"{state}\">\n\
             \x20       <method_environment>\n\
             \x20         <envvar name=\"SP_EMU_STATE_DIR\" value=\"{state}\"/>\n\
             \x20         <envvar name=\"SP_EMU_BOARD\" value=\"{}\"/>\n\
             \x20         <envvar name=\"SP_EMU_BRIDGE\" value=\"[{addr}]:{}\"/>\n\
             \x20         <envvar name=\"SP_EMU_VPD_SERIAL\" value=\"{}\"/>\n\
             \x20         <envvar name=\"SP_EMU_NO_DEBUG\" value=\"1\"/>\n\
             \x20         <envvar name=\"SP_EMU_HOST_POWER\" value=\"[{addr}]:{}\"/>\n",
            board,
            sp.base_port,
            sp.serial,
            sp.power_port()
        ));
        if sp.role == voxel_config::sp::SpRole::Sidecar {
            s.push_str(&format!(
                "            <envvar name=\"SP_EMU_IGNITION\" value=\"{ignition}\"/>\n"
            ));
        }
        if let Some(part) = &sp.part_number {
            s.push_str(&format!(
                "            <envvar name=\"SP_EMU_VPD_PART\" value=\"{part}\"/>\n"
            ));
        }
        if rot {
            s.push_str(&format!(
                "            <envvar name=\"SP_EMU_ROT_FLASH\" value=\"{dir}/rot.image\"/>\n"
            ));
            if dir.join("bootleby.zip").exists() {
                s.push_str(&format!(
                    "            <envvar name=\"SP_EMU_ROT_BOOTLEBY\" value=\"{dir}/bootleby.zip\"/>\n"
                ));
            } else {
                s.push_str(
                    "            <envvar name=\"SP_EMU_ROT_NO_BOOTLEBY\" value=\"1\"/>\n",
                );
            }
        }
        s.push_str(
            "          </method_environment>\n\
             \x20     </method_context>\n\
             \x20   </exec_method>\n\
             \x20   <exec_method type=\"method\" name=\"stop\" exec=\":kill\" timeout_seconds=\"30\"/>\n\
             \x20   <property_group name=\"startd\" type=\"framework\">\n\
             \x20     <propval name=\"duration\" type=\"astring\" value=\"child\"/>\n\
             \x20   </property_group>\n\
             \x20 </instance>\n",
        );
    }
    s.push_str("</service>\n</service_bundle>\n");
    s
}

/// Whether link already carries an IPv6 link-local. ipadm rejects a global v6
/// address on a link without one.
fn has_link_local(link: &str) -> bool {
    let Some(out) =
        probe_out("ipadm", &["show-addr", "-p", "-o", "addrobj,addr"])
    else {
        return false;
    };
    let prefix = format!("{link}/");
    out.lines().any(|l| l.starts_with(&prefix) && l.contains("fe80"))
}

/// Import a manifest until every instance it declares exists. svccfg can lose
/// a race against startd's repository writes and import the service alone.
pub(crate) fn import_manifest(
    path: &str,
    fmris: &[String],
) -> anyhow::Result<()> {
    let mut waited = 0;
    loop {
        let imported = run(&["svccfg", "import", path]).is_ok()
            && fmris.iter().all(|f| probe("svcs", &["-H", f]));
        if imported {
            return Ok(());
        }
        if waited >= IMPORT_WAIT_S {
            bail!("svccfg import {path}: instances missing after {waited}s");
        }
        std::thread::sleep(std::time::Duration::from_secs(2));
        waited += 2;
    }
}

// The running fleet, for the operator commands.

/// A rack's staged fleet directory: binary, archives, RoT image, flashed state.
/// Absolute, because sp-emu resolves relative paths against its own cwd.
pub(crate) fn fleet_dir(rack: usize) -> camino::Utf8PathBuf {
    let rel = crate::topo::sp_fleet_dir(rack).join("sp-emu");
    if let Ok(abs) = rel.canonicalize_utf8() {
        return abs;
    }
    // Not staged yet: absolutize against the anchored workdir.
    match std::env::current_dir()
        .ok()
        .and_then(|d| camino::Utf8PathBuf::from_path_buf(d).ok())
    {
        Some(cwd) => cwd.join(rel.strip_prefix("./").unwrap_or(&rel)),
        None => rel,
    }
}

/// The sp-emu binary driving a rack's fleet.
pub(crate) fn emu_bin(rack: usize) -> anyhow::Result<camino::Utf8PathBuf> {
    let bin = fleet_dir(rack).join("sp-emu");
    if !bin.exists() {
        bail!("no sp-emu at {bin}; is this a running --emu rack?");
    }
    Ok(bin)
}

/// One SP's flashed state directory.
pub(crate) fn state_dir(rack: usize, port: u16) -> camino::Utf8PathBuf {
    fleet_dir(rack).join("state").join(port.to_string())
}

/// One SP's SMF instance.
pub(crate) fn fmri(rack: usize, port: u16) -> String {
    format!("{SVC}:{}", instance(rack, port))
}

/// Restart one SP to pick up new flash or a changed environment.
pub(crate) fn restart(rack: usize, port: u16) -> bool {
    probe("pfexec", &["svcadm", "restart", &fmri(rack, port)])
}

/// Enable or disable one SP, as ignition powering its sled on or off would.
pub(crate) fn set_enabled(rack: usize, port: u16, on: bool) -> bool {
    let verb = if on { "enable" } else { "disable" };
    probe("pfexec", &["svcadm", verb, "-s", &fmri(rack, port)])
}

/// Restart every SP in a rack's fleet, returning how many were restarted.
pub(crate) fn restart_rack(rack: usize) -> usize {
    let prefix = format!("{SVC}:r{rack}sp");
    let Some(out) = probe_out("svcs", &["-H", "-o", "fmri", SVC]) else {
        return 0;
    };
    out.split_whitespace()
        .filter(|f| f.starts_with(&prefix))
        .filter(|f| probe("pfexec", &["svcadm", "restart", f]))
        .count()
}

/// Re-flash one SP's slot A from image and bring it back. The state dir
/// survives, and the instance is re-enabled even when the flash fails.
pub(crate) fn flash_sp(
    rack: usize,
    port: u16,
    image: &Utf8Path,
) -> anyhow::Result<()> {
    let bin = emu_bin(rack)?;
    let fmri = fmri(rack, port);
    run(&["svcadm", "disable", "-s", &fmri])?;
    let flashed = Command::new(bin.as_str())
        .args(["flash", "a", image.as_str()])
        .env("SP_EMU_STATE_DIR", state_dir(rack, port).as_str())
        .status()
        .with_context(|| format!("spawn sp-emu flash for port {port}"))?;
    let enabled = run(&["svcadm", "enable", &fmri]);
    if !flashed.success() {
        bail!("sp-emu flash failed for port {port} ({flashed})");
    }
    enabled
}

/// One SP's SMF start/environment as svcprop prints it. None when the
/// instance is absent.
pub(crate) fn read_env(rack: usize, port: u16) -> Option<String> {
    probe_out("svcprop", &["-p", "start/environment", &fmri(rack, port)])
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Replace one SP's SMF start/environment and restart it. Tokens are quoted
/// individually so bracketed addresses survive svccfg.
pub(crate) fn set_env(
    rack: usize,
    port: u16,
    tokens: &[String],
) -> anyhow::Result<()> {
    let fmri = fmri(rack, port);
    let quoted: Vec<String> =
        tokens.iter().map(|t| format!("\"{t}\"")).collect();
    let body = format!(
        "select {fmri}\nsetprop start/environment = astring: ({})\n",
        quoted.join(" ")
    );
    let file =
        crate::util::temp_dir().join(format!("voxel-sp-env-{port}.scfg"));
    std::fs::write(&file, body).with_context(|| format!("write {file}"))?;
    let applied = run(&["svccfg", "-f", file.as_str()])
        .and_then(|_| run(&["svcadm", "refresh", &fmri]))
        .and_then(|_| run(&["svcadm", "restart", &fmri]));
    let _ = std::fs::remove_file(&file);
    applied.with_context(|| format!("apply the new environment to {fmri}"))
}

/// Bring up one rack's fleet: the fleet address on link, a flashed state
/// directory per SP, then the SMF instances.
pub(crate) fn up(
    cfg: &voxel_config::VoxelConfig,
    rack: usize,
    fleet: &voxel_config::sp::SpFleet,
    rot: bool,
    dir: &Utf8Path,
    link: &str,
) -> anyhow::Result<()> {
    let addr = voxel_config::config::sp_host_addr(rack);
    let prefix_len = voxel_config::config::SP_NET_PREFIX_LEN;
    let fleet = fleet.emu_sps();
    // Only called for --emu. An empty fleet is a caller bug and would wedge
    // the rack at the Nexus handoff.
    if fleet.is_empty() {
        bail!("--emu but rack {rack} has no emulated SPs");
    }
    let dir = dir
        .canonicalize_utf8()
        .with_context(|| format!("resolve fleet dir {dir}"))?;
    let bin = dir.join("sp-emu");
    if !bin.exists() {
        bail!(
            "--emu needs an sp-emu binary on the host: set [sp].emu_bin \
             (the fleet runs here now, not in the switch zone)"
        );
    }
    // A global v6 address needs a link-local. Link-local only: no site prefix
    // is adopted. Every rack shares it, and down() leaves it.
    if !has_link_local(link) {
        run(&[
            "ipadm",
            "create-addr",
            "-t",
            "-T",
            "addrconf",
            "-p",
            "stateless=no,stateful=no",
            &format!("{link}/voxelll"),
        ])
        .with_context(|| format!("add an IPv6 link-local on {link}"))?;
    }
    // A relaunch re-adds the same address and ipadm refuses to create over
    // an existing addrobj.
    let obj = format!("{link}/{}", addrobj(rack));
    probe("pfexec", &["ipadm", "delete-addr", &obj]);
    run(&[
        "ipadm",
        "create-addr",
        "-t",
        "-T",
        "static",
        "-a",
        &format!("{addr}/{prefix_len}"),
        &obj,
    ])
    .with_context(|| format!("add rack {rack} SP fleet address {addr}"))?;

    // A switch zone answers from its bootstrap address. Route each scrimlet's
    // bootstrap /64 via its SP address; on-link, so it installs before boot.
    for s in cfg.sleds().iter().filter(|s| s.rack == rack && s.scrimlet) {
        let dest = s.bootstrap_subnet();
        let via = voxel_config::config::sp_scrimlet_addr(rack, s.index);
        probe("pfexec", &["route", "delete", "-inet6", &dest, &via]);
        run(&["route", "add", "-inet6", &dest, &via]).with_context(|| {
            format!("route {dest} back to scrimlet {} via {via}", s.name)
        })?;
    }

    for sp in &fleet {
        let state = dir.join("state").join(sp.base_port.to_string());
        std::fs::create_dir_all(&state)
            .with_context(|| format!("mkdir {state}"))?;
        let archive = dir.join(format!("{}.archive", board_of(sp)));
        let st = Command::new(bin.as_str())
            .args(["flash", "a", archive.as_str()])
            .env("SP_EMU_STATE_DIR", state.as_str())
            .status()
            .with_context(|| {
                format!("spawn sp-emu flash for port {}", sp.base_port)
            })?;
        if !st.success() {
            bail!("sp-emu flash failed for port {} ({st})", sp.base_port);
        }
        // Seed the host boot QSPI with the release's phase 1, the 32 MiB array
        // sp-emu persists, so host phase 1 identifies instead of reading blank.
        let rom = dir.join("host-phase1.rom");
        if board_of(sp) == "gimlet" && rom.exists() {
            std::fs::copy(&rom, state.join("qspi-flash.bin")).with_context(
                || format!("seed host phase 1 for port {}", sp.base_port),
            )?;
        }
    }

    let path = manifest_path(rack);
    let body = manifest(rack, &dir, &addr, rot, &fleet);
    let tmp = dir.join("voxel-sp-emu.xml");
    std::fs::write(&tmp, body).with_context(|| format!("write {tmp}"))?;
    run(&["cp", tmp.as_str(), &path])?;
    let fmris: Vec<String> =
        fleet.iter().map(|sp| fmri(rack, sp.base_port)).collect();
    import_manifest(&path, &fmris).with_context(|| {
        format!(
            "sp-emu fleet NOT started: no SMF instances for rack {rack}; MGS \
             would find no SPs"
        )
    })?;
    let ports: Vec<u16> = fleet.iter().map(|sp| sp.base_port).collect();
    println!(
        "[voxel] rack {rack} SP fleet up on {addr} ({} SP(s): {ports:?})",
        ports.len()
    );
    Ok(())
}

/// Remove the host's route to dest by the gateway the kernel recorded. A global
/// next hop is stored as its link-local neighbour, and route needs the pair.
fn delete_route(dest: &str) {
    let Some(out) = probe_out("netstat", &["-rn", "-f", "inet6"]) else {
        return;
    };
    for line in out.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.first() == Some(&dest) && f.len() >= 2 {
            probe("pfexec", &["route", "delete", "-inet6", dest, f[1]]);
        }
    }
}

/// Tear down one rack's fleet, leaving other racks alone. Best effort.
pub(crate) fn down(cfg: &voxel_config::VoxelConfig, rack: usize) {
    for s in cfg.sleds().iter().filter(|s| s.rack == rack && s.scrimlet) {
        delete_route(&s.bootstrap_subnet());
    }
    let prefix = format!("{SVC}:r{rack}sp");
    if let Some(out) = probe_out("svcs", &["-H", "-o", "fmri", SVC]) {
        for fmri in out.split_whitespace().filter(|f| f.starts_with(&prefix)) {
            probe("pfexec", &["svcadm", "disable", "-s", fmri]);
            probe("pfexec", &["svccfg", "delete", "-f", fmri]);
        }
    }
    // The addrobj name is unique per rack. Find whichever link carries it.
    let obj = addrobj(rack);
    if let Some(out) = probe_out("ipadm", &["show-addr", "-p", "-o", "addrobj"])
    {
        for a in out.split_whitespace().filter(|a| a.ends_with(&obj)) {
            probe("pfexec", &["ipadm", "delete-addr", a]);
        }
    }
    probe("pfexec", &["rm", "-f", &manifest_path(rack)]);
}

/// The host link for a rack's fleet address: the voxel segment in isolated
/// mode, else the link carrying the default route.
fn host_link(cfg: &voxel_config::VoxelConfig) -> anyhow::Result<String> {
    if cfg.external.isolated() {
        return Ok(crate::isolated_external::VNIC.to_string());
    }
    let out = probe_out("netstat", &["-rn", "-f", "inet"])
        .context("read the host routing table")?;
    for line in out.lines() {
        // Columns: Destination Gateway Flags Ref Use Interface. Gateway routes
        // have no interface column.
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.first() == Some(&"default") && f.len() >= 6 {
            return Ok(f[f.len() - 1].to_string());
        }
    }
    bail!(
        "no default route: cannot tell which host link reaches the sleds' LAN"
    )
}

/// Bring up every rack's fleet.
pub(crate) fn up_all(
    cfg: &voxel_config::VoxelConfig,
    rot: bool,
) -> anyhow::Result<()> {
    let link = host_link(cfg)?;
    for rack in 0..cfg.topology.racks() {
        up(
            cfg,
            rack,
            &crate::topo::emu_fleet(cfg, rack),
            rot,
            // stage_sp_emu writes the fleet into an sp-emu subdirectory.
            &crate::topo::sp_fleet_dir(rack).join("sp-emu"),
            &link,
        )?;
    }
    Ok(())
}

/// Tear down every rack's fleet. Best effort.
pub(crate) fn down_all(cfg: &voxel_config::VoxelConfig) {
    for rack in 0..cfg.topology.racks() {
        down(cfg, rack);
    }
}
