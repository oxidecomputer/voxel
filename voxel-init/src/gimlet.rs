//! Gimlet (sled) bring-up—replaces `gimlet-launch.sh`. Runs in the voxel-cp
//! helios guest. The control plane is already installed (`/opt/oxide`); this
//! applies the per-launch / topology bits that can't be baked: ephemeral virtual
//! hardware, the detected underlay NICs, the generated sled + RSS configs, the
//! switch1 identity for the 2nd scrimlet, then activates the control plane (which
//! kicks RSS on the RSS node).

use crate::sys::{
    note, read_external_net, replace_in_file, run, run_env, run_quiet, warn,
};
use anyhow::{Context, Result, bail};
use camino::Utf8Path;
use indoc::{formatdoc, indoc};
use std::fs;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::Duration;

const CARGO_BAY: &str = "/opt/cargo-bay";
const OMICRON: &str = "/opt/oxide/omicron";
// `<CARGO_BAY>/sled-config.toml` (kept literal: `concat!` can't expand a const).
const SLED_CFG: &str = "/opt/cargo-bay/sled-config.toml";
const PATCHED_CFG: &str = "/tmp/sled-config.toml";

/// Pick the `staged` path if it exists on disk, else fall back to `baked` — the
/// "dev cargo-bay wins, baked image otherwise" rule used to source every sp-emu
/// artifact.
fn pick(staged: String, baked: String) -> String {
    if Utf8Path::new(&staged).exists() { staged } else { baked }
}

/// Poll `f` every 2s until it returns true or `max_s` seconds elapse; returns
/// whether the condition was met (callers decide whether a timeout is fatal).
fn wait_until(max_s: u32, mut f: impl FnMut() -> bool) -> bool {
    let mut waited = 0;
    while !f() {
        if waited >= max_s {
            return false;
        }
        std::thread::sleep(Duration::from_secs(2));
        waited += 2;
    }
    true
}

pub fn bring_up() -> Result<()> {
    setup_ssh();
    crash_dump();
    maybe_load_sidecar();

    // The omicron CLI tools are baked into the image at /opt/oxide/omicron, and
    // xtask/omicron-package run relative to that tree.
    if !Utf8Path::new(OMICRON).exists() {
        bail!("{OMICRON} not baked into the image");
    }
    std::env::set_current_dir(OMICRON)
        .with_context(|| format!("cd {OMICRON}"))?;
    let xtask_bin = format!("{OMICRON}/xtask");
    let xtask_dl = format!("{OMICRON}/xtask-downloader");

    let (underlay, other) = detect_underlay();
    patch_sled_config(&underlay)?;
    setup_external_networking(&other);
    setup_virtual_hardware();
    inject_runtime_configs()?;
    unplumb_softnpu_source();
    maybe_start_switch_enforcer()?;

    // Activate the (already-unpacked) control plane. On the RSS node this kicks RSS.
    // omicron-package reads XTASK_BIN / XTASK_DOWNLOADER_BIN from the environment.
    if !run_env(
        "./omicron-package",
        &["activate"],
        &[("XTASK_BIN", &xtask_bin), ("XTASK_DOWNLOADER_BIN", &xtask_dl)],
    ) {
        warn("omicron-package activate failed");
    }
    note("gimlet bring-up complete");
    Ok(())
}

/// SSH convenience for `voxel host login` (was the sourced `setup_ssh`
/// function). illumos sshd defaults differ from debian's, hence the explicit
/// config edits.
fn setup_ssh() {
    let authorized = format!("{CARGO_BAY}/root_authorized_keys");
    if Utf8Path::new(&authorized).exists() {
        let _ = fs::create_dir_all("/root/.ssh");
        if let Ok(keys) = fs::read(&authorized) {
            use std::io::Write;
            match fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open("/root/.ssh/authorized_keys")
            {
                Ok(mut f) => {
                    if let Err(e) = f.write_all(&keys) {
                        warn(format!("authorized_keys: {e}"));
                    }
                }
                Err(e) => warn(format!("authorized_keys: {e}")),
            }
        }
    }
    run("ssh-keygen", &["-A"]);
    replace_in_file(
        "/etc/ssh/sshd_config",
        &[
            ("#PasswordAuthentication no", "PasswordAuthentication yes"),
            ("#PermitEmptyPasswords no", "PermitEmptyPasswords yes"),
            ("PermitRootLogin without-password", "PermitRootLogin yes"),
        ],
    );
    run("svcadm", &["restart", "svc:/network/ssh:default"]);
}

fn crash_dump() {
    run("zfs", &["create", "-p", "-V", "8G", "rpool/dump"]);
    run("dumpadm", &["-d", "/dev/zvol/dsk/rpool/dump"]);
}

/// Scrimlets load the baked SoftNPU sidecar P4 program. Gimlets have no softnpu
/// device, so `scadm propolis load-program` would fail there—gate on sled_mode.
fn maybe_load_sidecar() {
    let scrimlet = fs::read_to_string(SLED_CFG)
        .map(|s| s.contains(r#"sled_mode = "scrimlet""#))
        .unwrap_or(false);
    if scrimlet {
        run(
            "/opt/oxide/sidecar/scadm",
            &[
                "propolis",
                "load-program",
                "/opt/oxide/sidecar/libsidecar_lite.so",
            ],
        );
    }
}

/// The Oxide underlay is jumbo (MTU 9000). The guest vioif ordering is
/// topology-dependent (scrimlet vs gimlet, sled count), so we can't hardcode
/// names: probe `vioif0..7`—the ones that accept MTU 9000 are the underlay, the
/// rest are ext / host-LAN candidates.
fn detect_underlay() -> (Vec<String>, Vec<String>) {
    let mut underlay = Vec::new();
    let mut other = Vec::new();
    for n in 0..8 {
        let nic = format!("vioif{n}");
        if !run_quiet("dladm", &["show-link", &nic]) {
            continue;
        }
        if run_quiet("dladm", &["set-linkprop", "-t", "-p", "mtu=9000", &nic]) {
            underlay.push(nic);
        } else {
            other.push(nic);
        }
    }
    note(format!("underlay(jumbo)={underlay:?} ext-candidates={other:?}"));
    (underlay, other)
}

/// Patch this sled's config to the detected underlay links (the generated config
/// ships placeholders), write the patched copy to /tmp, and seed the xtask
/// WORKSPACE config (`smf/sled-agent/non-gimlet/config.toml`) that
/// virtual-hardware reads. Uses `toml_edit`—no `sed`.
fn patch_sled_config(underlay: &[String]) -> Result<()> {
    let text = fs::read_to_string(SLED_CFG)
        .with_context(|| format!("read {SLED_CFG}"))?;
    let mut doc: toml_edit::DocumentMut =
        text.parse().with_context(|| format!("parse {SLED_CFG}"))?;
    if let Some(first) = underlay.first() {
        doc["data_link"] = toml_edit::value(first.as_str());
        // Substitute the detected NICs into whatever data_links SHAPE the staged
        // config has, so this agent works on any image: an inline table (omicron
        // main's `{ kind = "virtual", devices = [...] }`) keeps its `kind` and
        // only its `devices` are replaced; a bare array (pre-main) is rewritten.
        let mut devices = toml_edit::Array::new();
        for u in underlay {
            devices.push(u.as_str());
        }
        let dl = &mut doc["data_links"];
        if let Some(table) = dl.as_inline_table_mut() {
            table.insert("devices", toml_edit::Value::Array(devices));
        } else {
            *dl = toml_edit::value(devices);
        }
    }
    fs::write(PATCHED_CFG, doc.to_string())
        .with_context(|| format!("write {PATCHED_CFG}"))?;
    // xtask virtual-hardware reads the workspace config (vdevs + sled_mode).
    let workspace = "smf/sled-agent/non-gimlet/config.toml";
    fs::copy(PATCHED_CFG, workspace)
        .with_context(|| format!("seed {workspace}"))?;
    Ok(())
}

/// Bring up the non-underlay NICs that reach the host LAN—but never vioif0,
/// the SoftNPU packet source the switch zone must claim (plumbing it in the GZ
/// makes oxz_switch fail "interface used in the global zone").
///
/// Isolated mode (voxel-managed segment) stages a static address in
/// `/opt/cargo-bay/ external-net`, applying it to the first non-vioif0 NIC and
/// using the staged DNS list. `lan` mode falls back to DHCP + a hardcoded
/// resolver.
fn setup_external_networking(other: &[String]) {
    if let Some(ext) = read_external_net() {
        let resolv: String =
            ext.dns.iter().map(|s| format!("nameserver {s}\n")).collect();
        if let Err(e) = fs::write("/etc/resolv.conf", resolv) {
            warn(format!("resolv.conf: {e}"));
        }

        match other.iter().find(|ifc| ifc.as_str() != "vioif0") {
            Some(ifc) => {
                // Falcon keeps the sled disk across destroy/relaunch, so a
                // prior launch's static address persists in /etc/ipadm/. `ipadm
                // create-addr` refuses to add over an existing addrobj, so wipe
                // any leftover /v4 addr before staging the current one. Silent
                // on absence (first launch, or a manual pre-wipe).
                run_quiet("ipadm", &["delete-addr", &format!("{ifc}/v4")]);
                run(
                    "ipadm",
                    &[
                        "create-addr",
                        "-T",
                        "static",
                        "-a",
                        &ext.ip_cidr,
                        &format!("{ifc}/v4"),
                    ],
                );
                // Persist the route (-p). voxel-init runs at launch, not at
                // boot, so a plain `route add` is lost if the sled VM reboots
                // mid-run while the static addr above survives via
                // /etc/ipadm/.
                //
                // We clear prior persistent defaults first so that a relaunch
                // (or a gateway change) does not stack or strand entries in
                // /etc/inet/static_routes.
                clear_persistent_defaults();
                run("route", &["-p", "add", "default", &ext.gateway]);
            }
            None => {
                warn("external-net staged but no external NIC candidate found")
            }
        }
        return;
    }
    if let Err(e) = fs::write("/etc/resolv.conf", "nameserver 1.1.1.1\n") {
        warn(format!("resolv.conf: {e}"));
    }
    // A prior isolated run's persistent default would otherwise sit alongside
    // the DHCP default and can win out.
    clear_persistent_defaults();
    for ifc in other {
        if ifc == "vioif0" {
            continue;
        }
        // Wipe any leftover /v4 addrobj (same reason as the isolated branch:
        // a prior isolated run's static address persists across relaunches
        // and blocks the `-T dhcp` create). Silent on absence.
        run_quiet("ipadm", &["delete-addr", &format!("{ifc}/v4")]);
        run("ipadm", &["create-addr", "-T", "dhcp", &format!("{ifc}/v4")]);
    }
}

/// Delete every persistent default route, not just the one via the current
/// gateway. The sled disk survives destroy/relaunch, so a gateway change (or
/// an isolated to lan switch) would otherwise strand a stale default in
/// /etc/inet/static_routes pointing at a dead gateway.
fn clear_persistent_defaults() {
    let Ok(out) = Command::new("route").args(["-p", "show"]).output() else {
        return;
    };
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        // Lines look like: "persistent: route add default 172.30.199.199".
        let mut toks = line.split_whitespace().skip_while(|t| *t != "default");
        let (Some(_), Some(gw)) = (toks.next(), toks.next()) else {
            continue;
        };
        run_quiet("route", &["-p", "delete", "default", gw]);
    }
}

/// Ephemeral emulated U.2/M.2 (deliberately not baked). Wipe any vdevs from a
/// prior launch first—falcon keeps the sled disk across destroy/relaunch, so
/// stale vdevs carry the OLD rack's trust-quorum ledger + crucible/cockroach
/// data; reusing them makes a fresh launch falsely report "initialized". A clean
/// launch must start from fresh storage.
fn setup_virtual_hardware() {
    let softnpu = [("SOFTNPU_MODE", "propolis")];
    run_env("./xtask", &["virtual-hardware", "destroy"], &softnpu);
    wipe_vdevs();
    if !run_env("./xtask", &["virtual-hardware", "create"], &softnpu) {
        warn("virtual-hardware create failed");
    }
}

fn wipe_vdevs() {
    let entries = match fs::read_dir("/var/tmp") {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if p.extension().and_then(|x| x.to_str()) == Some("vdev") {
            let _ = fs::remove_file(&p);
        }
    }
}

/// Inject the (data-link-patched) sled config + the generated RSS config (the
/// latter present only on the RSS node) as the runtime sled-agent configs.
fn inject_runtime_configs() -> Result<()> {
    fs::copy(PATCHED_CFG, "/opt/oxide/sled-agent/pkg/config.toml")
        .context("inject sled-agent config.toml")?;
    let rss = format!("{CARGO_BAY}/config-rss.toml");
    if Utf8Path::new(&rss).exists() {
        fs::copy(&rss, "/opt/oxide/sled-agent/pkg/config-rss.toml")
            .context("inject config-rss.toml")?;
    }
    Ok(())
}

/// Force vioif0 (the SoftNPU pkt_source) unplumbed in the GZ—the switch zone
/// must claim it, but the softnpu fabric / DHCP keeps grabbing it. Harmless on
/// gimlets (vioif0 unused there).
fn unplumb_softnpu_source() {
    run_quiet("ipadm", &["delete-addr", "vioif0/v4"]);
    run_quiet("ipadm", &["delete-if", "vioif0"]);
}

const SWITCH_ZONE_MGS: &str =
    "/zone/oxz_switch/root/var/svc/manifest/site/mgs/config.toml";
const SWITCH_ZONE_SP: &str =
    "/zone/oxz_switch/root/var/svc/manifest/site/sp-sim/config.toml";

// sp-emu staging: `stage_config` drops the binary + a `<base_port>.flash` per
// emulated SP into this scrimlet's cargo-bay; voxel-init copies them into the
// switch zone and runs each as an SMF contract daemon.
const SP_EMU_CARGO_DIR: &str = "/opt/cargo-bay/sp-emu";
const SP_EMU_ZONE_DIR: &str = "/zone/oxz_switch/root/opt/oxide/sp-emu";
const SP_EMU_MANIFEST: &str =
    "/zone/oxz_switch/root/var/svc/manifest/site/voxel-sp-emu.xml";
// Each SP gets its OWN rot-serve (one RoT per SP — not shared). The rot-serve for
// SP base-port `P` listens on the zone-local port `P - SP_EMU_ROT_PORT_OFFSET`
// (e.g. 33300 -> 19300), clear of the SP base-port + ereport ranges.
const SP_EMU_ROT_PORT_OFFSET: u16 = 14000;

// The sidecar SP's MGS base port (voxel_config::sp::SP_PORT_BASE); every other
// port in the fleet manifest is a gimlet. voxel-init is cross-compiled (musl) and
// doesn't link voxel-config, so the value is mirrored here.
const SIDECAR_SP_PORT: u16 = 33300;

/// Bake-once: the image bakes switch0 + sp-sim for a fixed gimlet count, but this
/// launch may run a different count, and the 2nd scrimlet must present as switchN
/// anyway. `stage_config` generates this scrimlet's slot MGS config + sp-sim
/// config for the live count; if they're staged, spawn a detached watcher that
/// swaps them into the switch zone (+ bounces the services) as soon as it
/// extracts. Detached into its own session with stdio to a log so it doesn't hold
/// `voxel launch`'s exec pipe open. Runs on every scrimlet (slot from the staged
/// filename)—but it's a no-op when the baked configs already match (see
/// `switch_enforcer`), so a matched-count launch behaves exactly as before.
fn maybe_start_switch_enforcer() -> Result<()> {
    let Some(slot) = staged_switch_slot() else {
        return Ok(());
    };
    let exe = std::env::current_exe().context("current_exe")?;
    let log =
        fs::File::create("/tmp/switch-enforcer.log").context("enforcer log")?;
    let mut cmd = Command::new(exe);
    cmd.arg("switch-enforcer")
        .arg(slot.to_string())
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    // New session so it survives this exec returning (no SIGHUP) and so its own
    // fds—not the launch pipe—are all that hold its stdio.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let child = cmd.spawn().context("spawn switch-enforcer")?;
    note(format!(
        "switch{slot} enforcer started (pid {}), log /tmp/switch-enforcer.log",
        child.id()
    ));
    Ok(())
}

/// This node's switch slot, from the `mgs-config-switch{N}.toml` `stage_config`
/// dropped into its cargo-bay—or `None` if it isn't a scrimlet.
fn staged_switch_slot() -> Option<u8> {
    for entry in fs::read_dir(CARGO_BAY).ok()?.flatten() {
        let name = entry.file_name();
        let name = name.to_str()?;
        if let Some(rest) = name.strip_prefix("mgs-config-switch")
            && let Some(slot) =
                rest.strip_suffix(".toml").and_then(|d| d.parse::<u8>().ok())
        {
            return Some(slot);
        }
    }
    None
}

/// Whether the switch zone is fully installed and running. Before that, the
/// zone install can still rewrite the baked configs under us.
fn switch_zone_running() -> bool {
    std::process::Command::new("zoneadm")
        .args(["-z", "oxz_switch", "list", "-p"])
        .output()
        .is_ok_and(|o| {
            String::from_utf8_lossy(&o.stdout)
                .split(':')
                .nth(2)
                .is_some_and(|state| state == "running")
        })
}

/// The detached enforcer (runs as voxel-init switch-enforcer <slot>). Forces
/// this scrimlet's launch-count MGS (switch{slot}) + sp-sim configs into the
/// switch zone, restarting each service, until the live files match what we
/// staged. Judges nothing until the zone RUNS: the install's package
/// extraction rewrites the baked configs, so an early file match is
/// meaningless and an early restart fails. Output -> /tmp/switch-enforcer.log.
pub fn switch_enforcer(slot: u8) {
    let mgs_staged = format!("{CARGO_BAY}/mgs-config-switch{slot}.toml");
    let sp_staged = format!("{CARGO_BAY}/sp-sim-config.toml");
    let mut mgs_restarted = true;
    let mut sp_restarted = true;
    for _ in 0..1500 {
        // up to ~25 min safety net
        if !switch_zone_running() || !Utf8Path::new(SWITCH_ZONE_MGS).exists() {
            std::thread::sleep(Duration::from_secs(1));
            continue;
        }
        let mgs_ok = files_equal(SWITCH_ZONE_MGS, &mgs_staged);
        let sp_present = Utf8Path::new(SWITCH_ZONE_SP).exists();
        // No staged sp-sim config (e.g. --emu, where setup_sp_emu disables sp-sim)
        // -> nothing for the enforcer to reconcile.
        let sp_ok = !Utf8Path::new(&sp_staged).exists()
            || !sp_present
            || files_equal(SWITCH_ZONE_SP, &sp_staged);
        if mgs_ok && sp_ok && mgs_restarted && sp_restarted {
            note(format!("switch{slot} + sp-sim configs in place"));
            break;
        }
        if !mgs_ok || !mgs_restarted {
            if !mgs_ok && let Err(e) = fs::copy(&mgs_staged, SWITCH_ZONE_MGS) {
                warn(format!("copy switch{slot} MGS config: {e}"));
            }
            mgs_restarted = run(
                "zlogin",
                &["oxz_switch", "svcadm", "restart", "svc:/oxide/mgs:default"],
            );
        }
        if sp_present && (!sp_ok || !sp_restarted) {
            if !sp_ok && let Err(e) = fs::copy(&sp_staged, SWITCH_ZONE_SP) {
                warn(format!("copy sp-sim config: {e}"));
            }
            sp_restarted = run(
                "zlogin",
                &[
                    "oxz_switch",
                    "svcadm",
                    "restart",
                    "svc:/oxide/sp-sim:default",
                ],
            );
        }
        note(format!("forced switch{slot} / sp-sim configs"));
        std::thread::sleep(Duration::from_secs(1));
    }
    // Stand up any staged emulated SPs as an SMF service in the switch zone (the
    // zone is up by now). Idempotent + reboot-safe (startd owns them).
    setup_sp_emu();
}

fn files_equal(a: &str, b: &str) -> bool {
    matches!((fs::read(a), fs::read(b)), (Ok(x), Ok(y)) if x == y)
}

/// Stand up the staged emulated SPs (`sp-emu`) as `svc:/oxide/voxel-sp-emu` in the
/// switch zone, replacing sp-sim for their ports. `stage_config` flashed one
/// `<base_port>.flash` per emu SP + staged the `sp-emu` binary into this
/// scrimlet's cargo-bay; here we copy them into the zone and import a manifest
/// with one contract-daemon instance per SP (startd supervises + restarts each,
/// survives reboots). No-op when nothing's staged; idempotent once imported.
fn setup_sp_emu() {
    // The emu fleet's CONTENT (binary, per-SP flashes, rot.flash) is either STAGED
    // in the cargo-bay (dev: [sp].emu_bin set -> topo flashes locally) or BAKED
    // into the image at /opt/oxide/sp-emu (self-contained). Staged wins; baked is
    // the fallback. The SP set + per-SP role + --emu-rot come from the `ports`
    // manifest topo ALWAYS stages, so we know the fleet even on the baked path;
    // for back-compat we also accept the legacy signal of staged `<port>.flash`
    // filenames.
    const BAKED: &str = "/opt/oxide/sp-emu";
    let staged_flashes: Vec<u16> = match fs::read_dir(SP_EMU_CARGO_DIR) {
        Ok(rd) => rd
            .flatten()
            .filter_map(|e| {
                e.file_name()
                    .to_str()
                    .and_then(|n| n.strip_suffix(".flash"))
                    .and_then(|p| p.parse::<u16>().ok())
            })
            .collect(),
        Err(_) => Vec::new(),
    };
    // Manifest: a `rot <0|1>` line then `<port> <role>` lines.
    let manifest = fs::read_to_string(format!("{SP_EMU_CARGO_DIR}/ports"))
        .unwrap_or_default();
    let mut roles: std::collections::BTreeMap<u16, String> =
        std::collections::BTreeMap::new();
    let mut rot_from_manifest = false;
    for line in manifest.lines() {
        let mut it = line.split_whitespace();
        match (it.next(), it.next()) {
            (Some("rot"), Some(v)) => rot_from_manifest = v == "1",
            (Some(p), Some(role)) => {
                if let Ok(p) = p.parse::<u16>() {
                    roles.insert(p, role.to_string());
                }
            }
            _ => {}
        }
    }
    let mut ports: Vec<u16> = if !staged_flashes.is_empty() {
        staged_flashes
    } else {
        roles.keys().copied().collect()
    };
    if ports.is_empty() {
        return; // no emu SPs on this scrimlet
    }
    ports.sort_unstable();
    // Copy the binary + flash files into the zone (idempotent; safe to redo).
    if let Err(e) = fs::create_dir_all(SP_EMU_ZONE_DIR) {
        warn(format!("mkdir {SP_EMU_ZONE_DIR}: {e}"));
        return;
    }
    let bin_to = format!("{SP_EMU_ZONE_DIR}/sp-emu");
    let bin_src =
        pick(format!("{SP_EMU_CARGO_DIR}/sp-emu"), format!("{BAKED}/sp-emu"));
    if let Err(e) = fs::copy(&bin_src, &bin_to) {
        warn(format!("copy sp-emu binary from {bin_src}: {e}"));
        return;
    }
    run("chmod", &["+x", &bin_to]);
    for p in &ports {
        // Staged <port>.flash wins; else the baked per-role flash. Gimlet flashes
        // are identical (the per-SP serial is set at runtime from SP_EMU_BRIDGE),
        // so one baked gimlet.flash serves every gimlet port; 33300 -> sidecar.
        let role = roles.get(p).map(String::as_str).unwrap_or("gimlet");
        let src = pick(
            format!("{SP_EMU_CARGO_DIR}/{p}.flash"),
            format!("{BAKED}/{role}.flash"),
        );
        if let Err(e) = fs::copy(&src, format!("{SP_EMU_ZONE_DIR}/{p}.flash")) {
            warn(format!("copy {p}.flash from {src}: {e}"));
        }
    }
    // Copy the RoT image too, if --emu-rot. Each SP gets its OWN out-of-process
    // RoT: `sp_emu_manifest` emits one `voxel-rot-emu` rot-serve instance per SP
    // (its own oxide-rot-1 on a dedicated zone-local port) and points each SP at
    // it via SP_EMU_ROT_SERVICE. One sled/sidecar -> one SP -> one RoT (not
    // shared, not in-process, not deferred). SPs stay single-core (RoT
    // out-of-process), so they answer MGS `switch-id` during RSS and the RoT is
    // live from boot -> MGS/Nexus pin the real RoT at rack-init. Enabled if a
    // rot.flash is staged OR the manifest flags it (the baked path).
    let staged_rot = format!("{SP_EMU_CARGO_DIR}/rot.flash");
    let rot_enabled = Utf8Path::new(&staged_rot).exists() || rot_from_manifest;
    if rot_enabled {
        let rot_src = pick(staged_rot, format!("{BAKED}/rot.flash"));
        if let Err(e) =
            fs::copy(&rot_src, format!("{SP_EMU_ZONE_DIR}/rot.flash"))
        {
            warn(format!("copy rot.flash from {rot_src}: {e}"));
        }
    }
    if let Err(e) =
        fs::write(SP_EMU_MANIFEST, sp_emu_manifest(&ports, rot_enabled))
    {
        warn(format!("write sp-emu manifest: {e}"));
        return;
    }
    // Wait until oxz_switch is actually bootable (zlogin works) before any svc
    // ops — early in bring-up the zone is still 'incomplete'.
    if !wait_until(180, || run_quiet("zlogin", &["oxz_switch", "true"])) {
        warn("sp-emu: oxz_switch never became ready; skipping");
        return;
    }
    // Whole-fleet emu: every SP is emulated, so sp-sim isn't needed. Disable it
    // (releasing the shared ports) before sp-emu binds them. Wait for sp-sim to be
    // imported first, else the disable is a no-op and the baked sp-sim races us.
    let _ = wait_until(60, || {
        run_quiet(
            "zlogin",
            &["oxz_switch", "svcs", "svc:/oxide/sp-sim:default"],
        )
    });
    run(
        "zlogin",
        &["oxz_switch", "svcadm", "disable", "-s", "svc:/oxide/sp-sim:default"],
    );
    run(
        "zlogin",
        &[
            "oxz_switch",
            "svccfg",
            "import",
            "/var/svc/manifest/site/voxel-sp-emu.xml",
        ],
    );
    note(format!(
        "sp-emu fleet up ({} SP(s): {ports:?}); sp-sim disabled",
        ports.len()
    ));
}

/// SMF manifest for the emulated SP fleet, each instance running the locked
/// sp-emu launch line in the FOREGROUND (no `&`/nohup) so startd's contract owns
/// it -> restart-on-crash + reboot-safety. Board/flash/bridge are passed via the
/// method environment. Port 33300 is the sidecar; any other port is a gimlet.
///
/// When `rot` is set (--emu-rot), the bundle also emits a `svc:/oxide/voxel-rot-emu`
/// service with ONE rot-serve instance PER SP — each running its own oxide-rot-1
/// on a dedicated zone-local port — and points each SP at ITS OWN RoT via
/// SP_EMU_ROT_SERVICE (with a require_all dep so the RoTs start + prewarm first).
/// One sled/sidecar -> one SP -> one RoT: separate process, own state — not
/// shared, not in-process, not deferred. Each RoT runs out-of-process, so its SP
/// stays single-core and answers MGS `switch-id` during RSS -> RoT live from boot,
/// MGS/Nexus pin the real RoT at rack-init. When `rot` is false the SPs run with
/// their canned RoT, as before.
fn sp_emu_manifest(ports: &[u16], rot: bool) -> String {
    let mut s = indoc! {r#"
        <?xml version="1.0"?>
        <!DOCTYPE service_bundle SYSTEM "/usr/share/lib/xml/dtd/service_bundle.dtd.1">
        <service_bundle type="manifest" name="voxel-sp-emu">
    "#}
    .to_string();
    // Per-SP RoT services: one oxide-rot-1 per SP, each on its own zone-local port.
    if rot {
        s.push_str(indoc! {r#"
            <service name="oxide/voxel-rot-emu" type="service" version="1">
              <dependency name="multi_user" grouping="require_all" restart_on="none" type="service">
                <service_fmri value="svc:/milestone/multi-user:default"/>
              </dependency>
        "#});
        for &port in ports {
            let rport = port - SP_EMU_ROT_PORT_OFFSET;
            s.push_str(&formatdoc! {r#"
                <instance name="rot{port}" enabled="true">
                  <exec_method type="method" name="start" exec="/opt/oxide/sp-emu/sp-emu rot-serve [::1]:{rport} /opt/oxide/sp-emu/rot.flash" timeout_seconds="0"/>
                  <exec_method type="method" name="stop" exec=":kill" timeout_seconds="30"/>
                  <property_group name="startd" type="framework">
                    <propval name="duration" type="astring" value="child"/>
                  </property_group>
                </instance>
            "#});
        }
        s.push_str("</service>\n");
    }
    s.push_str(indoc! {r#"
        <service name="oxide/voxel-sp-emu" type="service" version="1">
          <dependency name="multi_user" grouping="require_all" restart_on="none" type="service">
            <service_fmri value="svc:/milestone/multi-user:default"/>
          </dependency>
    "#});
    if rot {
        // require_all on the whole RoT service: every per-SP rot-serve must be up.
        s.push_str(indoc! {r#"
            <dependency name="rot" grouping="require_all" restart_on="none" type="service">
              <service_fmri value="svc:/oxide/voxel-rot-emu"/>
            </dependency>
        "#});
    }
    for &port in ports {
        let board = if port == SIDECAR_SP_PORT { "sidecar" } else { "gimlet" };
        s.push_str(&formatdoc! {r#"
            <instance name="sp{port}" enabled="true">
              <exec_method type="method" name="start" exec="/opt/oxide/sp-emu/sp-emu gdb a 340000000" timeout_seconds="0">
                <method_context>
                  <method_environment>
                    <envvar name="SP_EMU_BOARD" value="{board}"/>
                    <envvar name="SP_EMU_FLASH" value="/opt/oxide/sp-emu/{port}.flash"/>
                    <envvar name="SP_EMU_BRIDGE" value="[::1]:{port}"/>
        "#});
        // Point the SP at ITS OWN rot-serve (one RoT per SP): single-core SP +
        // out-of-process RoT, live from boot through RSS—no two-core wedge.
        if rot {
            let rport = port - SP_EMU_ROT_PORT_OFFSET;
            s.push_str(&format!(
                "        <envvar name=\"SP_EMU_ROT_SERVICE\" value=\"[::1]:{rport}\"/>\n"
            ));
        }
        s.push_str(indoc! {r#"
                    <envvar name="SP_EMU_NO_DEBUG" value="1"/>
                    <envvar name="SP_EMU_IDLE_MS" value="20"/>
                  </method_environment>
                </method_context>
              </exec_method>
              <exec_method type="method" name="stop" exec=":kill" timeout_seconds="30"/>
              <property_group name="startd" type="framework">
                <propval name="duration" type="astring" value="child"/>
              </property_group>
            </instance>
        "#});
    }
    s.push_str("</service>\n</service_bundle>\n");
    s
}

/// SMF-service entry point—the baked `svc:/oxide/voxel-switch-enforcer`, run on
/// **every boot**. This is the reboot/restart-safe path: the one-shot detached
/// enforcer (`maybe_start_switch_enforcer`) is lost if the sled is restarted or
/// its process is killed under load mid-bring-up—and then the scrimlet silently
/// reverts to the baked switch0, which wedges that rack's Nexus handoff
/// ("switch-port qsfp0 not found"). As an SMF service, startd re-runs it at every
/// boot and restarts it if it dies, so the slot identity can't be silently lost.
/// It reads the desired slot from the (persistent, host-backed) cargo-bay, so it's
/// a no-op on gimlets and on switch0 (content-equality), and idempotent if the
/// detached enforcer already applied it.
pub fn switch_enforcer_svc() {
    // The cargo-bay 9p mount is present from boot on a real sled; on the image
    // BUILD VM it never appears, so bail fast rather than hang the build/boot.
    let mut waited = 0;
    while !Utf8Path::new(SLED_CFG).exists() {
        if waited >= 30 {
            note("switch-enforcer-svc: no cargo-bay mount; nothing to enforce");
            return;
        }
        std::thread::sleep(Duration::from_secs(2));
        waited += 2;
    }
    match staged_switch_slot() {
        Some(slot) => {
            note(format!("switch-enforcer-svc: enforcing switch{slot}"));
            switch_enforcer(slot);
        }
        None => note(
            "switch-enforcer-svc: no switch slot staged (gimlet); nothing to do",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sp_emu_manifest_structure_is_balanced() {
        let m = sp_emu_manifest(&[33300, 33310], true);
        assert_eq!(m.matches("<instance name=\"sp").count(), 2);
        assert_eq!(m.matches("<instance name=\"rot").count(), 2);
        assert_eq!(
            m.matches("<service ").count(),
            m.matches("</service>").count()
        );
        assert_eq!(
            m.matches("<instance ").count(),
            m.matches("</instance>").count()
        );
        assert!(m.contains("SP_EMU_ROT_SERVICE"));
        assert!(m.contains("SP_EMU_BOARD\" value=\"sidecar\""));

        let plain = sp_emu_manifest(&[33310], false);
        assert!(!plain.contains("voxel-rot-emu"));
        assert!(!plain.contains("SP_EMU_ROT_SERVICE"));
        assert!(plain.contains("SP_EMU_BOARD\" value=\"gimlet\""));
    }
}
