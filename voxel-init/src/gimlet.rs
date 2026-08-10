//! Gimlet (sled) bring-up - replaces `gimlet-launch.sh`. Runs in the voxel-cp
//! helios guest. The control plane is already installed (`/opt/oxide`); this
//! applies the per-launch / topology bits that can't be baked: ephemeral virtual
//! hardware, the detected underlay NICs, the generated sled + RSS configs, the
//! switch1 identity for the 2nd scrimlet, then activates the control plane (which
//! kicks RSS on the RSS node).

use crate::sys::{
    ExternalNet, capture, capture_required, note, read_external_net,
    replace_in_file, run, run_env, run_env_required, run_quiet, run_required,
    warn,
};
use anyhow::{Context, Result, anyhow};
use std::fs;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

const CARGO_BAY: &str = "/opt/cargo-bay";
const OMICRON: &str = "/opt/oxide/omicron";
// `<CARGO_BAY>/sled-config.toml` (kept literal: `concat!` can't expand a const).
const SLED_CFG: &str = "/opt/cargo-bay/sled-config.toml";
const PATCHED_CFG: &str = "/tmp/sled-config.toml";
const GIMLET_COMPLETE_SENTINEL: &str = "gimlet bring-up complete";

/// Pick the `staged` path if it exists on disk, else fall back to `baked` — the
/// "dev cargo-bay wins, baked image otherwise" rule used to source every sp-emu
/// artifact.
fn pick(staged: String, baked: String) -> String {
    if std::path::Path::new(&staged).exists() { staged } else { baked }
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
    finish_bring_up(bring_up_required(), |line| note(line))
}

fn bring_up_required() -> Result<()> {
    setup_ssh();
    crash_dump();
    maybe_load_sidecar();

    // The omicron CLI tools are baked into the image at /opt/oxide/omicron, and
    // xtask/omicron-package run relative to that tree.
    if !Path::new(OMICRON).exists() {
        return Err(anyhow!("{OMICRON} not baked into the image"));
    }
    std::env::set_current_dir(OMICRON)
        .with_context(|| format!("cd {OMICRON}"))?;

    let (underlay, other) = detect_underlay();
    if underlay.is_empty() {
        return Err(anyhow!(
            "detected zero jumbo-capable underlay devices; refusing to patch sled configuration"
        ));
    }
    patch_sled_config(&underlay)?;
    setup_external_networking(&other)?;
    setup_virtual_hardware()?;
    tune_guest_zfs()?;
    inject_runtime_configs()?;
    unplumb_softnpu_source();
    maybe_start_switch_enforcer()?;

    // Activate the (already-unpacked) control plane. On the RSS node this kicks RSS.
    let xtask_bin = format!("{OMICRON}/xtask");
    let xtask_dl = format!("{OMICRON}/xtask-downloader");
    run_env_required(
        "./omicron-package",
        &["activate"],
        &[("XTASK_BIN", &xtask_bin), ("XTASK_DOWNLOADER_BIN", &xtask_dl)],
    )?;
    Ok(())
}

fn finish_bring_up(
    required_steps: Result<()>,
    mut emit: impl FnMut(&str),
) -> Result<()> {
    required_steps?;
    emit(GIMLET_COMPLETE_SENTINEL);
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
            match fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open("/root/.ssh/authorized_keys")
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
    let text = fs::read_to_string(SLED_CFG)
        .with_context(|| format!("read {SLED_CFG}"))?;
    let patched = patch_sled_config_text(&text, underlay)
        .with_context(|| format!("patch {SLED_CFG}"))?;
    fs::write(PATCHED_CFG, patched)
        .with_context(|| format!("write {PATCHED_CFG}"))?;
    // xtask virtual-hardware reads the workspace config (vdevs + sled_mode).
    let workspace = "smf/sled-agent/non-gimlet/config.toml";
    fs::copy(PATCHED_CFG, workspace)
        .with_context(|| format!("seed {workspace}"))?;
    Ok(())
}

/// Apply the required underlay substitutions to the staged config shapes Voxel
/// supported before manifested image contracts were introduced.
fn patch_sled_config_text(text: &str, underlay: &[String]) -> Result<String> {
    let first = underlay.first().ok_or_else(|| {
        anyhow!("cannot patch sled config with an empty underlay device set")
    })?;

    let mut doc: toml_edit::DocumentMut =
        text.parse().context("parse sled config")?;
    let data_links = doc
        .get_mut("data_links")
        .ok_or_else(|| anyhow!("missing data_links"))?;

    fn replace_array(array: &mut toml_edit::Array, underlay: &[String]) {
        array.clear();
        for device in underlay {
            array.push(device.as_str());
        }
    }

    match data_links {
        toml_edit::Item::Value(toml_edit::Value::Array(array)) => {
            replace_array(array, underlay);
        }
        toml_edit::Item::Value(toml_edit::Value::InlineTable(table)) => {
            let mut devices = toml_edit::Array::new();
            replace_array(&mut devices, underlay);
            table.insert("devices", toml_edit::Value::Array(devices));
        }
        _ => return Err(anyhow!("unsupported data_links shape")),
    }

    doc["data_link"] = toml_edit::value(first.as_str());
    Ok(doc.to_string())
}

/// DHCP the non-underlay NICs that reach the host LAN - but never vioif0, the
/// SoftNPU packet source the switch zone must claim (plumbing it in the GZ makes
/// oxz_switch fail "interface used in the global zone").
enum ExternalNetworkMode {
    Lan,
    Isolated(ExternalNet),
}

fn setup_external_networking(other: &[String]) -> Result<()> {
    let mode = match read_external_net()? {
        Some(ext) => ExternalNetworkMode::Isolated(ext),
        None => ExternalNetworkMode::Lan,
    };
    configure_external_network(
        mode,
        other,
        |cmd, args| {
            if cmd == "route" && args == ["-p", "show"] {
                capture_required(cmd, args)
            } else if cmd == "ipadm" && args.first() == Some(&"delete-addr") {
                run_quiet(cmd, args);
                Ok(String::new())
            } else {
                run_required(cmd, args).map(|()| String::new())
            }
        },
        |path, text| {
            fs::write(path, text).with_context(|| format!("write {path}"))
        },
    )
}

fn configure_external_network(
    mode: ExternalNetworkMode,
    other: &[String],
    mut command: impl FnMut(&str, &[&str]) -> Result<String>,
    mut write: impl FnMut(&str, &str) -> Result<()>,
) -> Result<()> {
    let clear_defaults = |command: &mut dyn FnMut(
        &str,
        &[&str],
    ) -> Result<String>|
     -> Result<()> {
        let output = command("route", &["-p", "show"])?;
        for line in output.lines() {
            let mut tokens =
                line.split_whitespace().skip_while(|token| *token != "default");
            if let (Some(_), Some(gateway)) = (tokens.next(), tokens.next()) {
                command("route", &["-p", "delete", "default", gateway])?;
            }
        }
        Ok(())
    };
    match mode {
        ExternalNetworkMode::Isolated(ext) => {
            if ext.iface.is_some() {
                note("ignoring router-only external-net iface on gimlet");
            }
            let interface = other
                .iter()
                .find(|ifc| ifc.as_str() != "vioif0")
                .ok_or_else(|| {
                anyhow!(
                    "external-net staged but no external NIC candidate found"
                )
            })?;
            write(
                "/etc/resolv.conf",
                &ext.dns
                    .iter()
                    .map(|dns| format!("nameserver {dns}\n"))
                    .collect::<String>(),
            )?;
            let address = format!("{interface}/v4");
            command("ipadm", &["delete-addr", &address])?;
            command(
                "ipadm",
                &["create-addr", "-T", "static", "-a", &ext.ip_cidr, &address],
            )?;
            clear_defaults(&mut command)?;
            command("route", &["-p", "add", "default", &ext.gateway])?;
        }
        ExternalNetworkMode::Lan => {
            write("/etc/resolv.conf", "nameserver 1.1.1.1\n")?;
            clear_defaults(&mut command)?;
            for interface in other.iter().filter(|ifc| ifc.as_str() != "vioif0")
            {
                let address = format!("{interface}/v4");
                command("ipadm", &["delete-addr", &address])?;
                command("ipadm", &["create-addr", "-T", "dhcp", &address])?;
            }
        }
    }
    Ok(())
}

/// Ephemeral emulated U.2/M.2 (deliberately not baked). Wipe any vdevs from a
/// prior launch first - falcon keeps the sled disk across destroy/relaunch, so
/// stale vdevs carry the OLD rack's trust-quorum ledger + crucible/cockroach
/// data; reusing them makes a fresh launch falsely report "initialized". A clean
/// launch must start from fresh storage.
fn setup_virtual_hardware() -> Result<()> {
    let softnpu = [("SOFTNPU_MODE", "propolis")];
    // Destroy + wipe the PERSISTENT /var/tmp first (falcon keeps the sled disk
    // across relaunch), so a prior launch's stale vdevs on rpool are gone.
    run_env("./xtask", &["virtual-hardware", "destroy"], &softnpu);
    wipe_vdevs();
    run_env_required("./xtask", &["virtual-hardware", "create"], &softnpu)
        .context("prepare virtual hardware")?;
    Ok(())
}

/// Cargo-bay flag (presence = on) for the guest ZFS-tuning lever (lever 3).
const WEAR_GUEST_ZFS_FLAG: &str = "/opt/cargo-bay/wear-guest-zfs";

fn lever_requested(path: &str) -> Result<bool> {
    Path::new(path)
        .try_exists()
        .with_context(|| format!("check requested lever flag {path:?}"))
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

/// Guest-side disk-wear tuning (lever 3), gated on the `wear-guest-zfs` cargo-bay
/// flag (`voxel launch` stages it from `disk_wear.guest_zfs_tuning`, default off).
/// voxel storage is fully ephemeral - the host pool may run `sync=disabled`
/// (lever 1) and `setup_virtual_hardware` wipes the vdevs every launch, so
/// nothing here is expected to survive a crash. Honoring the guest's constant
/// fsync/flushes with ZIL double-writes is therefore pure wasted NVMe wear on the
/// host (each guest sync becomes a ZIL write *and* a txg write, which the host
/// pool then amplifies again). Disabling sync + enabling compression on the
/// guest pools cuts that. Prefer lz4, but fall back to the pool's compatible
/// `compression=on` algorithm when an older image rpool lacks the lz4 feature.
///
/// The guest root pool (`rpool`) backs the `/var/tmp` vdev files, so tune it
/// immediately. The `oxi_*` internal and `oxp_*` external pools don't exist yet
/// - sled-agent creates them on the synthetic vdevs during RSS, after this
///
/// bring-up returns - so a detached watcher tunes each as it appears. These
/// properties only affect FUTURE writes, so tuning before RSS does its heavy
/// writing is what matters.
fn tune_guest_zfs() -> Result<()> {
    if !lever_requested(WEAR_GUEST_ZFS_FLAG)? {
        return Ok(());
    }
    let compression = tune_zfs_dataset("rpool")
        .context("apply requested guest ZFS tuning to rpool")?;
    note(format!(
        "guest ZFS tuning (lever 3): rpool sync=disabled compression={compression}"
    ));
    spawn_oxp_zfs_tuner().context("start requested oxp ZFS tuner")?;
    Ok(())
}

fn tune_zfs_dataset(dataset: &str) -> Result<&'static str> {
    tune_zfs_dataset_with(dataset, |args| run_required("zfs", args))
}

fn tune_zfs_dataset_with(
    dataset: &str,
    mut set: impl FnMut(&[&str]) -> Result<()>,
) -> Result<&'static str> {
    set(&["set", "sync=disabled", dataset])
        .with_context(|| format!("set sync=disabled on {dataset}"))?;
    match set(&["set", "compression=lz4", dataset]) {
        Ok(()) => Ok("lz4"),
        Err(lz4_error) => {
            set(&["set", "compression=on", dataset]).with_context(|| {
                format!(
                    "set compression=on compatibility fallback on {dataset} after compression=lz4 failed: {lz4_error:#}"
                )
            })?;
            Ok("on")
        }
    }
}

const OXP_TUNER_LOG: &str = "/tmp/oxp-zfs-tuner.log";

/// Spawn the detached Omicron-pool tuner (`voxel-init zfs-tuner`). Mirrors the
/// switch-enforcer spawn: a new session (so it survives this exec returning)
/// with its stdio to a log, not the launch pipe.
fn spawn_oxp_zfs_tuner() -> Result<()> {
    let exe = std::env::current_exe().context("current_exe")?;
    let log = fs::File::create(OXP_TUNER_LOG).context("oxp tuner log")?;
    let mut cmd = Command::new(exe);
    cmd.arg("zfs-tuner")
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    // New session so it survives this exec returning (no SIGHUP) and holds only
    // its own stdio fds - not the launch pipe.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    let child = cmd.spawn().context("spawn zfs-tuner")?;
    note(format!(
        "oxp zfs-tuner started (pid {}), log {OXP_TUNER_LOG}",
        child.id()
    ));
    Ok(())
}

/// The detached tuner (runs as `voxel-init zfs-tuner`). sled-agent imports the
/// `oxi_*` internal and `oxp_*` external pools during RSS, after bring-up
/// returns; apply the guest-wear tuning to each non-`rpool` pool as it appears,
/// for up to ~25 min. Idempotent - each pool is tuned (and logged) once. Output
/// -> `/tmp/oxp-zfs-tuner.log`.
pub fn oxp_zfs_tuner() {
    let mut tuned: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    // 300 * 5s = 1500s (~25 min) safety net, matching the switch enforcer.
    for _ in 0..300 {
        if let Some(out) = capture("zpool", &["list", "-H", "-o", "name"]) {
            for pool in out.lines().map(str::trim) {
                if pool.is_empty() || pool == "rpool" || tuned.contains(pool) {
                    continue;
                }
                match tune_zfs_dataset(pool) {
                    Ok(compression) => {
                        note(format!(
                            "tuned {pool}: sync=disabled compression={compression}"
                        ));
                        tuned.insert(pool.to_string());
                    }
                    Err(error) => warn(format!("tune {pool}: {error:#}")),
                }
            }
        }
        std::thread::sleep(Duration::from_secs(5));
    }
    note(format!("oxp zfs-tuner exiting; tuned {} pool(s)", tuned.len()));
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
/// filename) - but it's a no-op when the baked configs already match (see
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
    // fds - not the launch pipe - are all that hold its stdio.
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
/// dropped into its cargo-bay - or `None` if it isn't a scrimlet.
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
            run(
                "zlogin",
                &["oxz_switch", "svcadm", "restart", "svc:/oxide/mgs:default"],
            );
        }
        if sp_present && !sp_ok {
            if let Err(e) = fs::copy(&sp_staged, SWITCH_ZONE_SP) {
                warn(format!("copy sp-sim config: {e}"));
            }
            run(
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
    let rot_enabled =
        std::path::Path::new(&staged_rot).exists() || rot_from_manifest;
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
    let mut s = String::new();
    s.push_str("<?xml version=\"1.0\"?>\n");
    s.push_str(
        "<!DOCTYPE service_bundle SYSTEM \"/usr/share/lib/xml/dtd/service_bundle.dtd.1\">\n",
    );
    s.push_str("<service_bundle type=\"manifest\" name=\"voxel-sp-emu\">\n");
    // Per-SP RoT services: one oxide-rot-1 per SP, each on its own zone-local port.
    if rot {
        s.push_str("  <service name=\"oxide/voxel-rot-emu\" type=\"service\" version=\"1\">\n");
        s.push_str("    <dependency name=\"multi_user\" grouping=\"require_all\" restart_on=\"none\" type=\"service\">\n");
        s.push_str("      <service_fmri value=\"svc:/milestone/multi-user:default\"/>\n");
        s.push_str("    </dependency>\n");
        for &port in ports {
            let rport = port - SP_EMU_ROT_PORT_OFFSET;
            s.push_str(&format!(
                "    <instance name=\"rot{port}\" enabled=\"true\">\n"
            ));
            s.push_str(&format!("      <exec_method type=\"method\" name=\"start\" exec=\"/opt/oxide/sp-emu/sp-emu rot-serve [::1]:{rport} /opt/oxide/sp-emu/rot.flash\" timeout_seconds=\"0\"/>\n"));
            s.push_str("      <exec_method type=\"method\" name=\"stop\" exec=\":kill\" timeout_seconds=\"30\"/>\n");
            s.push_str(
                "      <property_group name=\"startd\" type=\"framework\">\n",
            );
            s.push_str("        <propval name=\"duration\" type=\"astring\" value=\"child\"/>\n");
            s.push_str("      </property_group>\n");
            s.push_str("    </instance>\n");
        }
        s.push_str("  </service>\n");
    }
    s.push_str("  <service name=\"oxide/voxel-sp-emu\" type=\"service\" version=\"1\">\n");
    s.push_str("    <dependency name=\"multi_user\" grouping=\"require_all\" restart_on=\"none\" type=\"service\">\n");
    s.push_str(
        "      <service_fmri value=\"svc:/milestone/multi-user:default\"/>\n",
    );
    s.push_str("    </dependency>\n");
    if rot {
        // require_all on the whole RoT service: every per-SP rot-serve must be up.
        s.push_str("    <dependency name=\"rot\" grouping=\"require_all\" restart_on=\"none\" type=\"service\">\n");
        s.push_str(
            "      <service_fmri value=\"svc:/oxide/voxel-rot-emu\"/>\n",
        );
        s.push_str("    </dependency>\n");
    }
    for &port in ports {
        let board = if port == SIDECAR_SP_PORT { "sidecar" } else { "gimlet" };
        s.push_str(&format!(
            "    <instance name=\"sp{port}\" enabled=\"true\">\n"
        ));
        s.push_str("      <exec_method type=\"method\" name=\"start\" exec=\"/opt/oxide/sp-emu/sp-emu gdb a 340000000\" timeout_seconds=\"0\">\n");
        s.push_str(
            "        <method_context>\n          <method_environment>\n",
        );
        s.push_str(&format!(
            "            <envvar name=\"SP_EMU_BOARD\" value=\"{board}\"/>\n"
        ));
        s.push_str(&format!("            <envvar name=\"SP_EMU_FLASH\" value=\"/opt/oxide/sp-emu/{port}.flash\"/>\n"));
        s.push_str(&format!(
            "            <envvar name=\"SP_EMU_BRIDGE\" value=\"[::1]:{port}\"/>\n"
        ));
        // Point the SP at ITS OWN rot-serve (one RoT per SP): single-core SP +
        // out-of-process RoT, live from boot through RSS - no two-core wedge.
        if rot {
            let rport = port - SP_EMU_ROT_PORT_OFFSET;
            s.push_str(&format!(
                "            <envvar name=\"SP_EMU_ROT_SERVICE\" value=\"[::1]:{rport}\"/>\n"
            ));
        }
        s.push_str(
            "            <envvar name=\"SP_EMU_NO_DEBUG\" value=\"1\"/>\n",
        );
        s.push_str(
            "            <envvar name=\"SP_EMU_IDLE_MS\" value=\"20\"/>\n",
        );
        s.push_str(
            "          </method_environment>\n        </method_context>\n",
        );
        s.push_str("      </exec_method>\n");
        s.push_str("      <exec_method type=\"method\" name=\"stop\" exec=\":kill\" timeout_seconds=\"30\"/>\n");
        s.push_str(
            "      <property_group name=\"startd\" type=\"framework\">\n",
        );
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
        None => note(
            "switch-enforcer-svc: no switch slot staged (gimlet); nothing to do",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ExternalNetworkMode, GIMLET_COMPLETE_SENTINEL,
        configure_external_network, finish_bring_up, lever_requested,
        patch_sled_config_text, tune_zfs_dataset_with,
    };
    use crate::sys::ExternalNet;
    use anyhow::anyhow;

    fn patch(input: &str, underlay: &[&str]) -> anyhow::Result<String> {
        let underlay =
            underlay.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();
        patch_sled_config_text(input, &underlay)
    }

    #[test]
    fn replaces_legacy_array_without_changing_its_shape() {
        let output = patch(
            "data_link = \"old0\"\ndata_links = [\"old0\"]\nmarker = \"keep\"\n",
            &["vioif1", "vioif2"],
        )
        .unwrap();
        let doc = output.parse::<toml_edit::DocumentMut>().unwrap();

        assert_eq!(doc["data_link"].as_str(), Some("vioif1"));
        let links = doc["data_links"].as_array().unwrap();
        assert_eq!(
            links.iter().map(|v| v.as_str().unwrap()).collect::<Vec<_>>(),
            ["vioif1", "vioif2"]
        );
        assert_eq!(doc["marker"].as_str(), Some("keep"));
    }

    #[test]
    fn replaces_inline_tagged_table_devices_and_preserves_keys() {
        let output = patch(
            "data_link = \"old0\"\ndata_links = { kind = \"virtual\", devices = [\"old0\"], other = \"keep\" }\n",
            &["vioif5", "vioif6"],
        )
        .unwrap();
        let doc = output.parse::<toml_edit::DocumentMut>().unwrap();
        let table = doc["data_links"].as_inline_table().unwrap();

        assert_eq!(table["kind"].as_str(), Some("virtual"));
        assert_eq!(table["other"].as_str(), Some("keep"));
        assert_eq!(
            table["devices"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap())
                .collect::<Vec<_>>(),
            ["vioif5", "vioif6"]
        );
    }

    #[test]
    fn rejects_empty_underlay_before_patching() {
        let input =
            "data_link = \"old0\"\ndata_links = 7 # deliberately unsupported\n";
        let error = patch(input, &[]).unwrap_err().to_string();
        assert!(error.contains("underlay"), "{error}");
    }

    #[test]
    fn lever_detection_propagates_metadata_errors() {
        let error =
            lever_requested("invalid\0lever-path").unwrap_err().to_string();
        assert!(error.contains("invalid"), "{error}");
        assert!(error.contains("lever"), "{error}");
    }

    #[test]
    fn completion_sentinel_is_emitted_only_after_required_steps_succeed() {
        let mut emitted = Vec::new();
        let failed =
            finish_bring_up(Err(anyhow!("activation failed")), |line| {
                emitted.push(line.to_string())
            });
        assert!(failed.is_err());
        assert!(emitted.is_empty());

        finish_bring_up(Ok(()), |line| emitted.push(line.to_string())).unwrap();
        assert_eq!(emitted, [GIMLET_COMPLETE_SENTINEL]);
    }

    #[test]
    fn guest_zfs_tuning_falls_back_when_lz4_is_not_supported() {
        let mut requests = Vec::new();
        let compression = tune_zfs_dataset_with("rpool", |args| {
            requests.push(
                args.iter().map(|arg| (*arg).to_string()).collect::<Vec<_>>(),
            );
            if args.contains(&"compression=lz4") {
                Err(anyhow!("pool must be upgraded"))
            } else {
                Ok(())
            }
        })
        .unwrap();

        assert_eq!(compression, "on");
        assert_eq!(
            requests,
            [
                ["set", "sync=disabled", "rpool"],
                ["set", "compression=lz4", "rpool"],
                ["set", "compression=on", "rpool"],
            ]
        );
    }

    #[test]
    fn guest_zfs_tuning_prefers_lz4_and_fails_if_no_compression_is_supported() {
        let mut requests = Vec::new();
        let compression = tune_zfs_dataset_with("oxp_test", |args| {
            requests.push(
                args.iter().map(|arg| (*arg).to_string()).collect::<Vec<_>>(),
            );
            Ok(())
        })
        .unwrap();
        assert_eq!(compression, "lz4");
        assert_eq!(requests.len(), 2);

        let error = tune_zfs_dataset_with("rpool", |args| {
            if args.contains(&"sync=disabled") {
                Ok(())
            } else {
                Err(anyhow!("unsupported compression"))
            }
        })
        .unwrap_err();
        assert!(error.to_string().contains("compression=on"), "{error:#}");
    }

    #[test]
    fn rejects_missing_data_links() {
        let error = patch("data_link = \"old0\"\n", &["vioif1"]).unwrap_err();
        assert!(error.to_string().contains("missing data_links"), "{error:#}");
    }

    #[test]
    fn rejects_scalar_data_links() {
        let error = patch("data_links = \"old0\"\n", &["vioif1"]).unwrap_err();
        assert!(
            error.to_string().contains("unsupported data_links shape"),
            "{error:#}"
        );
    }

    #[test]
    fn isolated_network_selects_candidate_and_requires_static_dns_and_route() {
        use std::cell::RefCell;
        let ext = ExternalNet {
            ip_cidr: "172.30.1.10/24".into(),
            gateway: "172.30.1.1".into(),
            dns: vec!["9.9.9.9".into()],
            iface: None,
        };
        let commands = RefCell::new(Vec::new());
        configure_external_network(
            ExternalNetworkMode::Isolated(ext),
            &["vioif0".into(), "vioif3".into()],
            |cmd, args| {
                commands.borrow_mut().push(format!("{cmd} {}", args.join(" ")));
                Ok(String::new())
            },
            |_path, text| {
                commands.borrow_mut().push(format!("dns {text}"));
                Ok(())
            },
        )
        .unwrap();
        let commands = commands.borrow();
        assert!(
            commands
                .iter()
                .any(|c| c.contains("static -a 172.30.1.10/24 vioif3/v4"))
        );
        assert!(
            commands
                .iter()
                .any(|c| c.contains("route -p add default 172.30.1.1"))
        );
        assert!(commands.iter().any(|c| c == "dns nameserver 9.9.9.9\n"));
        assert!(!commands.iter().any(|c| c.contains("vioif0/v4")));
    }

    #[test]
    fn lan_clears_stale_default_before_dhcp() {
        let mut commands = Vec::new();
        configure_external_network(
            ExternalNetworkMode::Lan,
            &["vioif2".into()],
            |cmd, args| {
                commands.push(format!("{cmd} {}", args.join(" ")));
                Ok(if args == ["-p", "show"] {
                    "persistent: route add default 10.0.0.1".into()
                } else {
                    String::new()
                })
            },
            |_, _| Ok(()),
        )
        .unwrap();
        let delete =
            commands.iter().position(|c| c.contains("delete default")).unwrap();
        let dhcp = commands.iter().position(|c| c.contains("-T dhcp")).unwrap();
        assert!(delete < dhcp);
    }

    #[test]
    fn isolated_network_without_candidate_fails() {
        let ext = ExternalNet {
            ip_cidr: "1.2.3.4/24".into(),
            gateway: "1.2.3.1".into(),
            dns: vec![],
            iface: None,
        };
        let error = configure_external_network(
            ExternalNetworkMode::Isolated(ext),
            &["vioif0".into()],
            |_, _| Ok(String::new()),
            |_, _| Ok(()),
        )
        .unwrap_err();
        assert!(error.to_string().contains("candidate"));
    }
}
