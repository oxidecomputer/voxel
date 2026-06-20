//! Gimlet (sled) bring-up - replaces `gimlet-launch.sh`. Runs in the voxel-cp
//! helios guest. The control plane is already installed (`/opt/oxide`); this
//! applies the per-launch / topology bits that can't be baked: ephemeral virtual
//! hardware, the detected underlay NICs, the generated sled + RSS configs, the
//! switch1 identity for the 2nd scrimlet, then activates the control plane (which
//! kicks RSS on the RSS node).

use crate::sys::{note, run, run_quiet, warn};
use anyhow::{anyhow, Context, Result};
use std::fs;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

const CARGO_BAY: &str = "/opt/cargo-bay";
const OMICRON: &str = "/opt/oxide/omicron";
const SLED_CFG: &str = "/opt/cargo-bay/sled-config.toml";
const PATCHED_CFG: &str = "/tmp/sled-config.toml";

pub fn bring_up() -> Result<()> {
    setup_ssh();
    crash_dump();
    maybe_load_sidecar();

    // The omicron CLI tools are baked into the image at /opt/oxide/omicron, and
    // xtask/omicron-package run relative to that tree.
    if !Path::new(OMICRON).exists() {
        return Err(anyhow!("{OMICRON} not baked into the image"));
    }
    std::env::set_var("XTASK_BIN", format!("{OMICRON}/xtask"));
    std::env::set_var("XTASK_DOWNLOADER_BIN", format!("{OMICRON}/xtask-downloader"));
    std::env::set_current_dir(OMICRON).with_context(|| format!("cd {OMICRON}"))?;

    let (underlay, other) = detect_underlay();
    patch_sled_config(&underlay)?;
    setup_external_networking(&other);
    setup_virtual_hardware();
    inject_runtime_configs()?;
    unplumb_softnpu_source();
    maybe_start_switch_enforcer()?;

    // Activate the (already-unpacked) control plane. On the RSS node this kicks RSS.
    if !run("./omicron-package", &["activate"]) {
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
    if Path::new(&authorized).exists() {
        let _ = fs::create_dir_all("/root/.ssh");
        if let Ok(keys) = fs::read(&authorized) {
            use std::io::Write;
            match fs::OpenOptions::new().create(true).append(true).open("/root/.ssh/authorized_keys")
            {
                Ok(mut f) => {
                    let _ = f.write_all(&keys);
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

fn replace_in_file(path: &str, subs: &[(&str, &str)]) {
    let mut text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => {
            warn(format!("read {path}: {e}"));
            return;
        }
    };
    for (from, to) in subs {
        text = text.replace(from, to);
    }
    if let Err(e) = fs::write(path, text) {
        warn(format!("write {path}: {e}"));
    }
}

fn crash_dump() {
    run("zfs", &["create", "-p", "-V", "8G", "rpool/dump"]);
    run("dumpadm", &["-d", "/dev/zvol/dsk/rpool/dump"]);
}

/// Scrimlets load the baked SoftNPU sidecar P4 program. Gimlets have no softnpu
/// device, so `scadm propolis load-program` would fail there - gate on sled_mode.
fn maybe_load_sidecar() {
    let scrimlet = fs::read_to_string(SLED_CFG)
        .map(|s| s.contains(r#"sled_mode = "scrimlet""#))
        .unwrap_or(false);
    if scrimlet {
        run(
            "/opt/oxide/sidecar/scadm",
            &["propolis", "load-program", "/opt/oxide/sidecar/libsidecar_lite.so"],
        );
    }
}

/// The Oxide underlay is jumbo (MTU 9000). The guest vioif ordering is
/// topology-dependent (scrimlet vs gimlet, sled count), so we can't hardcode
/// names: probe `vioif0..7` - the ones that accept MTU 9000 are the underlay, the
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
/// virtual-hardware reads. Uses `toml_edit` - no `sed`.
fn patch_sled_config(underlay: &[String]) -> Result<()> {
    let text = fs::read_to_string(SLED_CFG).with_context(|| format!("read {SLED_CFG}"))?;
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
    fs::write(PATCHED_CFG, doc.to_string()).with_context(|| format!("write {PATCHED_CFG}"))?;
    // xtask virtual-hardware reads the workspace config (vdevs + sled_mode).
    let workspace = "smf/sled-agent/non-gimlet/config.toml";
    fs::copy(PATCHED_CFG, workspace).with_context(|| format!("seed {workspace}"))?;
    Ok(())
}

/// DHCP the non-underlay NICs that reach the host LAN - but never vioif0, the
/// SoftNPU packet source the switch zone must claim (plumbing it in the GZ makes
/// oxz_switch fail "interface used in the global zone").
fn setup_external_networking(other: &[String]) {
    if let Err(e) = fs::write("/etc/resolv.conf", "nameserver 1.1.1.1\n") {
        warn(format!("resolv.conf: {e}"));
    }
    for ifc in other {
        if ifc == "vioif0" {
            continue;
        }
        run("ipadm", &["create-addr", "-T", "dhcp", &format!("{ifc}/v4")]);
    }
}

/// Ephemeral emulated U.2/M.2 (deliberately not baked). Wipe any vdevs from a
/// prior launch first - falcon keeps the sled disk across destroy/relaunch, so
/// stale vdevs carry the OLD rack's trust-quorum ledger + crucible/cockroach
/// data; reusing them makes a fresh launch falsely report "initialized". A clean
/// launch must start from fresh storage.
fn setup_virtual_hardware() {
    std::env::set_var("SOFTNPU_MODE", "propolis");
    run("./xtask", &["virtual-hardware", "destroy"]);
    wipe_vdevs();
    if !run("./xtask", &["virtual-hardware", "create"]) {
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
    if Path::new(&rss).exists() {
        fs::copy(&rss, "/opt/oxide/sled-agent/pkg/config-rss.toml")
            .context("inject config-rss.toml")?;
    }
    Ok(())
}

/// Force vioif0 (the SoftNPU pkt_source) unplumbed in the GZ - the switch zone
/// must claim it, but the softnpu fabric / DHCP keeps grabbing it. Harmless on
/// gimlets (vioif0 unused there).
fn unplumb_softnpu_source() {
    run_quiet("ipadm", &["delete-addr", "vioif0/v4"]);
    run_quiet("ipadm", &["delete-if", "vioif0"]);
}

const SWITCH_ZONE_MGS: &str = "/zone/oxz_switch/root/var/svc/manifest/site/mgs/config.toml";
const SWITCH_ZONE_SP: &str = "/zone/oxz_switch/root/var/svc/manifest/site/sp-sim/config.toml";

// sp-emu staging: `stage_config` drops the binary + a `<base_port>.flash` per
// emulated SP into this scrimlet's cargo-bay; voxel-init copies them into the
// switch zone and runs each as an SMF contract daemon.
const SP_EMU_CARGO_DIR: &str = "/opt/cargo-bay/sp-emu";
const SP_EMU_ZONE_DIR: &str = "/zone/oxz_switch/root/opt/oxide/sp-emu";
const SP_EMU_MANIFEST: &str = "/zone/oxz_switch/root/var/svc/manifest/site/voxel-sp-emu.xml";

/// Bake-once: the image bakes switch0 + sp-sim for a fixed gimlet count, but this
/// launch may run a different count, and the 2nd scrimlet must present as switchN
/// anyway. `stage_config` generates this scrimlet's slot MGS config + sp-sim
/// config for the live count; if they're staged, spawn a detached watcher that
/// swaps them into the switch zone (+ bounces the services) as soon as it
/// extracts. Detached into its own session with stdio to a log so it doesn't hold
/// `voxel launch`'s exec pipe open. Runs on every scrimlet (slot from the staged
/// filename) - but it's a no-op when the baked configs already match (see
/// `switch_enforcer`), so a matched-count launch behaves exactly as before.
fn maybe_start_switch_enforcer() -> Result<()> {
    let Some(slot) = staged_switch_slot() else {
        return Ok(());
    };
    let exe = std::env::current_exe().context("current_exe")?;
    let log = fs::File::create("/tmp/switch-enforcer.log").context("enforcer log")?;
    let mut cmd = Command::new(exe);
    cmd.arg("switch-enforcer")
        .arg(slot.to_string())
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    // New session so it survives this exec returning (no SIGHUP) and so its own
    // fds - not the launch pipe - are all that hold its stdio.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let child = cmd.spawn().context("spawn switch-enforcer")?;
    note(format!("switch{slot} enforcer started (pid {}), log /tmp/switch-enforcer.log", child.id()));
    Ok(())
}

/// This node's switch slot, from the `mgs-config-switch{N}.toml` `stage_config`
/// dropped into its cargo-bay - or `None` if it isn't a scrimlet.
fn staged_switch_slot() -> Option<u8> {
    for entry in fs::read_dir(CARGO_BAY).ok()?.flatten() {
        let name = entry.file_name();
        let name = name.to_str()?;
        if let Some(rest) = name.strip_prefix("mgs-config-switch") {
            if let Some(slot) = rest.strip_suffix(".toml").and_then(|d| d.parse::<u8>().ok()) {
                return Some(slot);
            }
        }
    }
    None
}

/// The detached enforcer (runs as `voxel-init switch-enforcer <slot>`). Forces
/// this scrimlet's launch-count MGS (switch{slot}) + sp-sim configs into the
/// switch zone the moment it extracts, restarting each service, until the live
/// files match what we staged. Uses content-equality so it's a **no-op when the
/// baked configs already match the launch count** - only a true count/slot
/// mismatch triggers a swap + restart. Output -> /tmp/switch-enforcer.log.
pub fn switch_enforcer(slot: u8) {
    let mgs_staged = format!("{CARGO_BAY}/mgs-config-switch{slot}.toml");
    let sp_staged = format!("{CARGO_BAY}/sp-sim-config.toml");
    for _ in 0..1500 {
        // up to ~25 min safety net
        if !Path::new(SWITCH_ZONE_MGS).exists() {
            std::thread::sleep(Duration::from_secs(1));
            continue;
        }
        let mgs_ok = files_equal(SWITCH_ZONE_MGS, &mgs_staged);
        let sp_present = Path::new(SWITCH_ZONE_SP).exists();
        // No staged sp-sim config (e.g. --emu, where setup_sp_emu disables sp-sim)
        // -> nothing for the enforcer to reconcile.
        let sp_ok = !Path::new(&sp_staged).exists()
            || !sp_present
            || files_equal(SWITCH_ZONE_SP, &sp_staged);
        if mgs_ok && sp_ok {
            note(format!("switch{slot} + sp-sim configs in place"));
            break;
        }
        if !mgs_ok {
            if let Err(e) = fs::copy(&mgs_staged, SWITCH_ZONE_MGS) {
                warn(format!("copy switch{slot} MGS config: {e}"));
            }
            run("zlogin", &["oxz_switch", "svcadm", "restart", "svc:/oxide/mgs:default"]);
        }
        if sp_present && !sp_ok {
            if let Err(e) = fs::copy(&sp_staged, SWITCH_ZONE_SP) {
                warn(format!("copy sp-sim config: {e}"));
            }
            run("zlogin", &["oxz_switch", "svcadm", "restart", "svc:/oxide/sp-sim:default"]);
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
    // Staged ports (filenames are `<base_port>.flash`).
    let mut ports: Vec<u16> = match fs::read_dir(SP_EMU_CARGO_DIR) {
        Ok(rd) => rd
            .flatten()
            .filter_map(|e| {
                e.file_name()
                    .to_str()
                    .and_then(|n| n.strip_suffix(".flash"))
                    .and_then(|p| p.parse::<u16>().ok())
            })
            .collect(),
        Err(_) => return, // no emu SPs staged on this scrimlet
    };
    if ports.is_empty() {
        return;
    }
    ports.sort_unstable();
    // Copy the binary + flash files into the zone (idempotent; safe to redo).
    if let Err(e) = fs::create_dir_all(SP_EMU_ZONE_DIR) {
        warn(format!("mkdir {SP_EMU_ZONE_DIR}: {e}"));
        return;
    }
    let bin_to = format!("{SP_EMU_ZONE_DIR}/sp-emu");
    if let Err(e) = fs::copy(format!("{SP_EMU_CARGO_DIR}/sp-emu"), &bin_to) {
        warn(format!("copy sp-emu binary: {e}"));
        return;
    }
    run("chmod", &["+x", &bin_to]);
    for p in &ports {
        if let Err(e) = fs::copy(
            format!("{SP_EMU_CARGO_DIR}/{p}.flash"),
            format!("{SP_EMU_ZONE_DIR}/{p}.flash"),
        ) {
            warn(format!("copy {p}.flash: {e}"));
        }
    }
    if let Err(e) = fs::write(SP_EMU_MANIFEST, sp_emu_manifest(&ports)) {
        warn(format!("write sp-emu manifest: {e}"));
        return;
    }
    // Wait until oxz_switch is actually bootable (zlogin works) before any svc
    // ops — early in bring-up the zone is still 'incomplete'.
    let mut waited = 0;
    while !run_quiet("zlogin", &["oxz_switch", "true"]) {
        if waited >= 180 {
            warn("sp-emu: oxz_switch never became ready; skipping");
            return;
        }
        std::thread::sleep(Duration::from_secs(2));
        waited += 2;
    }
    // Whole-fleet emu: every SP is emulated, so sp-sim isn't needed. Disable it
    // (releasing the shared ports) before sp-emu binds them. Wait for sp-sim to be
    // imported first, else the disable is a no-op and the baked sp-sim races us.
    let mut waited = 0;
    while !run_quiet("zlogin", &["oxz_switch", "svcs", "svc:/oxide/sp-sim:default"]) {
        if waited >= 60 {
            break;
        }
        std::thread::sleep(Duration::from_secs(2));
        waited += 2;
    }
    run("zlogin", &["oxz_switch", "svcadm", "disable", "-s", "svc:/oxide/sp-sim:default"]);
    run("zlogin", &["oxz_switch", "svccfg", "import", "/var/svc/manifest/site/voxel-sp-emu.xml"]);
    note(format!("sp-emu fleet up ({} SP(s): {ports:?}); sp-sim disabled", ports.len()));
}

/// SMF manifest for `svc:/oxide/voxel-sp-emu`: one instance per emulated SP, each
/// running the locked sp-emu launch line in the FOREGROUND (no `&`/nohup) so
/// startd's contract owns it -> restart-on-crash + reboot-safety. Board/flash/
/// bridge are passed via the method environment. Port 33300 is the sidecar; any
/// other port is a gimlet.
fn sp_emu_manifest(ports: &[u16]) -> String {
    let mut s = String::new();
    s.push_str("<?xml version=\"1.0\"?>\n");
    s.push_str("<!DOCTYPE service_bundle SYSTEM \"/usr/share/lib/xml/dtd/service_bundle.dtd.1\">\n");
    s.push_str("<service_bundle type=\"manifest\" name=\"voxel-sp-emu\">\n");
    s.push_str("  <service name=\"oxide/voxel-sp-emu\" type=\"service\" version=\"1\">\n");
    s.push_str("    <dependency name=\"multi_user\" grouping=\"require_all\" restart_on=\"none\" type=\"service\">\n");
    s.push_str("      <service_fmri value=\"svc:/milestone/multi-user:default\"/>\n");
    s.push_str("    </dependency>\n");
    for &port in ports {
        let board = if port == 33300 { "sidecar" } else { "gimlet" };
        s.push_str(&format!("    <instance name=\"sp{port}\" enabled=\"true\">\n"));
        s.push_str("      <exec_method type=\"method\" name=\"start\" exec=\"/opt/oxide/sp-emu/sp-emu gdb a 340000000\" timeout_seconds=\"0\">\n");
        s.push_str("        <method_context>\n          <method_environment>\n");
        s.push_str(&format!("            <envvar name=\"SP_EMU_BOARD\" value=\"{board}\"/>\n"));
        s.push_str(&format!("            <envvar name=\"SP_EMU_FLASH\" value=\"/opt/oxide/sp-emu/{port}.flash\"/>\n"));
        s.push_str(&format!("            <envvar name=\"SP_EMU_BRIDGE\" value=\"[::1]:{port}\"/>\n"));
        s.push_str("            <envvar name=\"SP_EMU_NO_DEBUG\" value=\"1\"/>\n");
        s.push_str("            <envvar name=\"SP_EMU_IDLE_MS\" value=\"20\"/>\n");
        s.push_str("          </method_environment>\n        </method_context>\n");
        s.push_str("      </exec_method>\n");
        s.push_str("      <exec_method type=\"method\" name=\"stop\" exec=\":kill\" timeout_seconds=\"30\"/>\n");
        s.push_str("      <property_group name=\"startd\" type=\"framework\">\n");
        s.push_str("        <propval name=\"duration\" type=\"astring\" value=\"child\"/>\n");
        s.push_str("      </property_group>\n");
        s.push_str("    </instance>\n");
    }
    s.push_str("  </service>\n</service_bundle>\n");
    s
}

/// SMF-service entry point - the baked `svc:/oxide/voxel-switch-enforcer`, run on
/// **every boot**. This is the reboot/restart-safe path: the one-shot detached
/// enforcer (`maybe_start_switch_enforcer`) is lost if the sled is restarted or
/// its process is killed under load mid-bring-up - and then the scrimlet silently
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
    while !Path::new(SLED_CFG).exists() {
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
        None => note("switch-enforcer-svc: no switch slot staged (gimlet); nothing to do"),
    }
}
