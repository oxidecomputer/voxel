//! Gimlet (sled) bring-up—replaces `gimlet-launch.sh`. Runs in the voxel-cp
//! helios guest. The control plane is already installed (`/opt/oxide`); this
//! applies the per-launch / topology bits that can't be baked: ephemeral virtual
//! hardware, the detected underlay NICs, the generated sled + RSS configs, the
//! switch1 identity for the 2nd scrimlet, then activates the control plane (which
//! kicks RSS on the RSS node).

use crate::sys::{
    capture, note, read_external_net, replace_in_file, run, run_env, run_quiet,
    warn,
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
/// Written at bake by `image create --from-tuf`; selects the native (no
/// omicron tooling) bring-up paths.
const TUF_MARKER: &str = "/opt/oxide/.voxel-tuf";

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

    // TUF images carry no omicron tooling; their virtual hardware and
    // activation are handled natively below.
    let tuf = Utf8Path::new(TUF_MARKER).exists();
    if !tuf {
        // The omicron CLI tools are baked into the image at /opt/oxide/omicron,
        // and xtask/omicron-package run relative to that tree.
        if !Utf8Path::new(OMICRON).exists() {
            bail!("{OMICRON} not baked into the image");
        }
        std::env::set_current_dir(OMICRON)
            .with_context(|| format!("cd {OMICRON}"))?;
    }

    let (underlay, other) = detect_underlay();
    patch_sled_config(&underlay, tuf)?;
    setup_external_networking(&other);
    if tuf {
        setup_virtual_hardware_native()?;
    } else {
        setup_virtual_hardware();
    }
    preseed_install_datasets();
    inject_runtime_configs()?;
    unplumb_softnpu_source();
    maybe_start_switch_enforcer()?;

    if tuf {
        activate_native();
    } else {
        // Activate the (already-unpacked) control plane. On the RSS node this
        // kicks RSS. omicron-package reads XTASK_BIN / XTASK_DOWNLOADER_BIN
        // from the environment.
        let xtask_bin = format!("{OMICRON}/xtask");
        let xtask_dl = format!("{OMICRON}/xtask-downloader");
        if !run_env(
            "./omicron-package",
            &["activate"],
            &[("XTASK_BIN", &xtask_bin), ("XTASK_DOWNLOADER_BIN", &xtask_dl)],
        ) {
            warn("omicron-package activate failed");
        }
    }
    note("gimlet bring-up complete");
    Ok(())
}

/// TUF images stage no xtask. In propolis softnpu mode `virtual-hardware
/// create` reduces to creating the config's vdev files (xtask's own default
/// size); softnpu lives in the switch zone propolis and needs no host setup.
fn setup_virtual_hardware_native() -> Result<()> {
    const VDEV_SIZE: u64 = 20 << 30;
    wipe_vdevs();
    let text = fs::read_to_string(PATCHED_CFG)
        .with_context(|| format!("read {PATCHED_CFG}"))?;
    let doc: toml_edit::DocumentMut =
        text.parse().with_context(|| format!("parse {PATCHED_CFG}"))?;
    // The vdev list's location differs by schema era: top level `vdevs`, or
    // under the `external_disks` table.
    let vdevs = doc
        .get("vdevs")
        .or_else(|| doc.get("external_disks").and_then(|d| d.get("vdevs")))
        .and_then(|v| v.as_array())
        .with_context(|| format!("no vdevs array in {PATCHED_CFG}"))?;
    let mut n = 0;
    for v in vdevs {
        let Some(name) = v.as_str() else { continue };
        let path = if name.starts_with('/') {
            name.to_string()
        } else {
            format!("/var/tmp/{name}")
        };
        let f = fs::File::create(&path)
            .with_context(|| format!("create vdev {path}"))?;
        f.set_len(VDEV_SIZE).with_context(|| format!("size vdev {path}"))?;
        n += 1;
    }
    if n == 0 {
        bail!("no vdevs created from {PATCHED_CFG}");
    }
    note(format!("created {n} vdev files"));
    Ok(())
}

/// TUF images: sled-agent is prestaged; activation is importing its SMF
/// manifest now that the runtime configs are injected. The manifest's
/// default instance starts on import.
fn activate_native() {
    if !run("svccfg", &["import", "/opt/oxide/sled-agent/pkg/manifest.xml"]) {
        warn("svccfg import sled-agent manifest failed");
        return;
    }
    run_quiet("svcadm", &["enable", "svc:/oxide/sled-agent:default"]);
    note("sled-agent activated");
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
fn patch_sled_config(underlay: &[String], tuf: bool) -> Result<()> {
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
    // xtask virtual-hardware reads the workspace config (vdevs + sled_mode);
    // TUF images have neither xtask nor the workspace tree.
    if !tuf {
        let workspace = "smf/sled-agent/non-gimlet/config.toml";
        fs::copy(PATCHED_CFG, workspace)
            .with_context(|| format!("seed {workspace}"))?;
    }
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

/// Control-plane service zones that live in the install dataset (the
/// preset-independent set; global-zone software like switch/propolis is not
/// here). Their hashes must match the target-release TUF repo's zone artifacts.
const INSTALL_ZONES: &[&str] = &[
    "clickhouse.tar.gz",
    "clickhouse_keeper.tar.gz",
    "clickhouse_server.tar.gz",
    "cockroachdb.tar.gz",
    "crucible.tar.gz",
    "crucible_pantry.tar.gz",
    "external_dns.tar.gz",
    "internal_dns.tar.gz",
    "nexus.tar.gz",
    "ntp.tar.gz",
    "oximeter.tar.gz",
    "probe.tar.gz",
];

/// Corpus staged by `image create --from-tuf`: the target repo's own
/// measurement corpus artifacts, byte exact. Preferred over the embedded fake
/// corpus when present.
const STAGED_CORPUS: &str = "/opt/oxide/measurements";

/// Fixed fake measurement corpus, embedded so it is present without a build-time
/// bake. A non-empty measurement manifest is required for a sled to be eligible
/// for noop image-source conversion. Voxel TUF repos must carry this same corpus
/// so the hashes match.
const CORPUS: &[(&str, &[u8])] = &[
    (
        "fake-measurement-id-9830767c45f2a02210a177fabafafe2c84501039289483f72cec299b0c78dbcb.cbor",
        include_bytes!(
            "../corpus/fake-measurement-id-9830767c45f2a02210a177fabafafe2c84501039289483f72cec299b0c78dbcb.cbor"
        ),
    ),
    (
        "fake-measurement-id-ae9279e9135de75e4e137c6da7f939b5a2eae6d931a7f2205df930e37cd58096.cbor",
        include_bytes!(
            "../corpus/fake-measurement-id-ae9279e9135de75e4e137c6da7f939b5a2eae6d931a7f2205df930e37cd58096.cbor"
        ),
    ),
];

/// Pre-create and populate the M.2 install datasets before sled-agent adopts
/// them. sled-agent reads the install-dataset manifest exactly once at startup
/// and never reloads, so seeding after it starts would need a restart (which on
/// a scrimlet recreates oxz_switch and wedges the rack). Instead we run in the
/// window after `virtual-hardware create` made the vdevs but before
/// `omicron-package activate` starts sled-agent: create a pool on each M.2 vdev,
/// drop the zones + corpus into `install/`, and leave it imported. sled-agent's
/// adoption then finds the pool and preserves it, and its first read reports a
/// non-empty manifest, so the reconfigurator can noop-convert to the TUF repo
/// with no restart. The pool must NOT be exported: sled-agent's `zpool import`
/// has no `-d`, so it cannot locate an exported file-vdev pool; an imported one
/// yields "already created/imported", which its import handler accepts.
/// Best-effort: on any failure the sled falls back to the manual path.
fn preseed_install_datasets() {
    let mut vdevs: Vec<String> = Vec::new();
    if let Ok(entries) = fs::read_dir("/var/tmp") {
        for e in entries.flatten() {
            let p = e.path();
            let is_m2 = p
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("m2_") && n.ends_with(".vdev"));
            if is_m2 && let Some(s) = p.to_str() {
                vdevs.push(s.to_string());
            }
        }
    }
    if vdevs.is_empty() {
        warn("preseed: no m2 vdevs in /var/tmp; skipping install-dataset seed");
        return;
    }
    for vdev in &vdevs {
        let Some(uuid) =
            capture("uuidgen", &[]).map(|u| u.trim().to_lowercase())
        else {
            warn("preseed: uuidgen failed");
            continue;
        };
        let pool = format!("oxi_{uuid}");
        let mnt = format!("/pool/int/{uuid}/install");
        if !run("zpool", &["create", "-f", &pool, vdev]) {
            warn(format!("preseed: zpool create {pool} on {vdev} failed"));
            continue;
        }
        if !run(
            "zfs",
            &[
                "create",
                "-o",
                &format!("mountpoint={mnt}"),
                &format!("{pool}/install"),
            ],
        ) {
            warn(format!("preseed: zfs create {pool}/install failed"));
            run("zpool", &["destroy", "-f", &pool]);
            continue;
        }
        let meas = format!("{mnt}/measurements");
        let _ = fs::create_dir_all(&meas);
        for z in INSTALL_ZONES {
            let src = format!("/opt/oxide/{z}");
            if Utf8Path::new(&src).exists()
                && let Err(e) = fs::copy(&src, format!("{mnt}/{z}"))
            {
                warn(format!("preseed: copy {z}: {e}"));
            }
        }
        let mut staged = 0;
        if let Ok(entries) = Utf8Path::new(STAGED_CORPUS).read_dir_utf8() {
            for e in entries.flatten() {
                let name = e.file_name();
                match fs::copy(e.path(), format!("{meas}/{name}")) {
                    Ok(_) => staged += 1,
                    Err(e) => warn(format!("preseed: copy corpus {name}: {e}")),
                }
            }
        }
        if staged == 0 {
            for (name, bytes) in CORPUS {
                if let Err(e) = fs::write(format!("{meas}/{name}"), bytes) {
                    warn(format!("preseed: write corpus {name}: {e}"));
                }
            }
        }
        // Leave the pool imported: sled-agent's `zpool import -f` (no `-d`)
        // cannot find an exported file-vdev pool, but on an already-imported
        // one it gets "a pool with that name is already created/imported",
        // which its import handler treats as success, so adoption preserves it.
        note(format!("preseed: staged install dataset on {vdev} ({pool})"));
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

// sp-emu staging: `stage_config` drops the binary + per-role hubris archives (and
// the rot image) into this scrimlet's cargo-bay; voxel-init copies them into the
// switch zone, flashes a per-SP state dir, and runs each SP as an SMF daemon.
const SP_EMU_CARGO_DIR: &str = "/opt/cargo-bay/sp-emu";
const SP_EMU_ZONE_DIR: &str = "/zone/oxz_switch/root/opt/oxide/sp-emu";
const SP_EMU_MANIFEST: &str =
    "/zone/oxz_switch/root/var/svc/manifest/site/voxel-sp-emu.xml";

// The sp-emu dir as the switch zone sees it (SP_EMU_ZONE_DIR is the same tree
// from the sled global zone). The SMF service runs in-zone, so its env paths
// use this prefix.
const SP_EMU_IN_ZONE: &str = "/opt/oxide/sp-emu";

// One emulated SP in the fleet, from the `ports` manifest.
struct EmuSp {
    port: u16,
    board: String,
    serial: String,
    part: String,
}

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
    open_switch_zone_ssh();

    // A later sled-agent restart recreates oxz_switch and wipes the staged emu
    // fleet (baked sp-sim returns with the zone; the staged emu fleet does not).
    // Stay resident and re-stage it when that happens, so a scrimlet bounce
    // self-heals instead of wedging the rack. No-op on sp-sim racks.
    if emu_fleet_configured() {
        monitor_emu_fleet(slot);
    }
}

/// True on an emu rack: the `ports` manifest lists at least one emulated SP.
fn emu_fleet_configured() -> bool {
    fs::read_to_string(format!("{SP_EMU_CARGO_DIR}/ports"))
        .map(|m| {
            m.lines().any(|l| {
                let f: Vec<&str> = l.split_whitespace().collect();
                f.len() == 4 && f[0].parse::<u16>().is_ok()
            })
        })
        .unwrap_or(false)
}

/// The staged emu binary inside the switch zone. Absent means the zone was
/// recreated (a sled-agent restart) and the staged fleet was lost.
fn fleet_staged_in_zone() -> bool {
    Utf8Path::new(&format!("{SP_EMU_ZONE_DIR}/sp-emu")).exists()
}

/// Resident watch, in the global zone, so it survives switch-zone recreation.
/// When a sled-agent restart recreates oxz_switch and drops the emu fleet,
/// re-stage the fleet, re-assert this scrimlet's MGS slot, and recover the
/// softnpu fabric. Without this a scrimlet bounce loses the fleet, darkens the
/// dataplane, and wedges the rack.
fn monitor_emu_fleet(slot: u8) {
    let mgs_staged = format!("{CARGO_BAY}/mgs-config-switch{slot}.toml");
    loop {
        std::thread::sleep(Duration::from_secs(20));
        // Act only once the zone is back and the fleet is actually gone.
        if !switch_zone_running() || fleet_staged_in_zone() {
            continue;
        }
        note("switch zone recreated without emu fleet; re-staging");
        setup_sp_emu();
        if Utf8Path::new(&mgs_staged).exists()
            && !files_equal(SWITCH_ZONE_MGS, &mgs_staged)
        {
            if let Err(e) = fs::copy(&mgs_staged, SWITCH_ZONE_MGS) {
                warn(format!("re-copy switch{slot} MGS config: {e}"));
            }
            run(
                "zlogin",
                &["oxz_switch", "svcadm", "restart", "svc:/oxide/mgs:default"],
            );
        }
        recover_fabric();
        open_switch_zone_ssh();
        note("emu fleet re-staged after switch-zone recreation");
    }
}

/// Recover the softnpu dataplane after a switch-zone recreation: reload the P4
/// program into propolis, bounce dendrite/tfport (they bind the ASIC), kick
/// mg-ddm (underlay routes) and mgd (BGP). Mirrors the manual recipe; idempotent.
fn recover_fabric() {
    run(
        "/opt/oxide/sidecar/scadm",
        &["propolis", "load-program", "/opt/oxide/sidecar/libsidecar_lite.so"],
    );
    run(
        "zlogin",
        &[
            "oxz_switch",
            "svcadm",
            "restart",
            "svc:/oxide/dendrite:default",
            "svc:/oxide/tfport:default",
        ],
    );
    run("svcadm", &["restart", "svc:/oxide/mg-ddm:default"]);
    run(
        "zlogin",
        &["oxz_switch", "svcadm", "restart", "svc:/oxide/mgd:default"],
    );
}

fn files_equal(a: &str, b: &str) -> bool {
    matches!((fs::read(a), fs::read(b)), (Ok(x), Ok(y)) if x == y)
}

/// Paths to the switch zone's sshd_config and login defaults from the global
/// zone.
const SWITCH_ZONE_SSHD: &str = "/zone/oxz_switch/root/etc/ssh/sshd_config";
const SWITCH_ZONE_LOGIN: &str = "/zone/oxz_switch/root/etc/default/login";

/// Open the switch zone's sshd to the lab posture (root, empty password,
/// forwarding scoped to the commission API), mirroring the global-zone
/// `setup_ssh`. The commission API binds only in-zone loopback, so the host
/// reaches it by forwarding through this sshd. Idempotent.
fn open_switch_zone_ssh() {
    if !Utf8Path::new(SWITCH_ZONE_SSHD).exists() {
        return;
    }
    run("zlogin", &["oxz_switch", "passwd", "-d", "root"]);
    replace_in_file(
        SWITCH_ZONE_SSHD,
        &[
            ("PasswordAuthentication no", "PasswordAuthentication yes"),
            ("PermitEmptyPasswords no", "PermitEmptyPasswords yes"),
            ("PermitRootLogin no", "PermitRootLogin yes"),
            ("AllowTcpForwarding no", "AllowTcpForwarding yes"),
            ("PermitOpen none", "PermitOpen [::1]:12234"),
            ("AllowUsers wicket support\n", "AllowUsers wicket support root\n"),
        ],
    );
    // login rejects the now-empty root password under PASSREQ=YES, which
    // breaks bare `zlogin oxz_switch` (and `voxel tp login`); allow it.
    replace_in_file(SWITCH_ZONE_LOGIN, &[("PASSREQ=YES", "PASSREQ=NO")]);
    run(
        "zlogin",
        &["oxz_switch", "svcadm", "restart", "svc:/network/ssh:default"],
    );
}

/// Stand up the staged emulated SPs (`sp-emu`) as `svc:/oxide/voxel-sp-emu` in the
/// switch zone, replacing sp-sim for their ports. `stage_config` flashed one
/// `<base_port>.flash` per emu SP + staged the `sp-emu` binary into this
/// scrimlet's cargo-bay; here we copy them into the zone and import a manifest
/// with one contract-daemon instance per SP (startd supervises + restarts each,
/// survives reboots). No-op when nothing's staged; idempotent once imported.
fn setup_sp_emu() {
    // The emu fleet (binary + per-role hubris archives + rot image) is STAGED in
    // the cargo-bay (dev: [sp].emu_bin set) or BAKED at /opt/oxide/sp-emu. Staged
    // wins; baked is the fallback. The SP set, per-SP role, VPD identity, and
    // --emu-rot come from the `ports` manifest topo always stages.
    const BAKED: &str = "/opt/oxide/sp-emu";
    let manifest = fs::read_to_string(format!("{SP_EMU_CARGO_DIR}/ports"))
        .unwrap_or_default();
    let mut rot = false;
    let mut fleet: Vec<EmuSp> = Vec::new();
    for line in manifest.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        match f.as_slice() {
            ["rot", v] => rot = *v == "1",
            [port, board, serial, part] => {
                if let Ok(port) = port.parse::<u16>() {
                    fleet.push(EmuSp {
                        port,
                        board: board.to_string(),
                        serial: serial.to_string(),
                        part: part.to_string(),
                    });
                }
            }
            _ => {}
        }
    }
    if fleet.is_empty() {
        return; // no emu SPs on this scrimlet
    }
    fleet.sort_by_key(|s| s.port);
    // Copy the binary into the zone (idempotent; safe to redo).
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
    // Copy each role's hubris archive; the SP is flashed from it below.
    let roles: std::collections::BTreeSet<&str> =
        fleet.iter().map(|s| s.board.as_str()).collect();
    for role in &roles {
        let src = pick(
            format!("{SP_EMU_CARGO_DIR}/{role}.archive"),
            format!("{BAKED}/{role}.archive"),
        );
        if let Err(e) =
            fs::copy(&src, format!("{SP_EMU_ZONE_DIR}/{role}.archive"))
        {
            warn(format!("copy {role}.archive from {src}: {e}"));
        }
    }
    // The RoT (oxide-rot-1) now runs in-process inside each SP over sprot, from
    // this one image; there is no separate rot-serve service. A staged bootleby
    // enables secure boot; rot.image must then be self-signed.
    let mut bootleby = false;
    if rot {
        let src = pick(
            format!("{SP_EMU_CARGO_DIR}/rot.image"),
            format!("{BAKED}/rot.image"),
        );
        if let Err(e) = fs::copy(&src, format!("{SP_EMU_ZONE_DIR}/rot.image")) {
            warn(format!("copy rot.image from {src}: {e}"));
        }
        let bl_src = pick(
            format!("{SP_EMU_CARGO_DIR}/bootleby.zip"),
            format!("{BAKED}/bootleby.zip"),
        );
        if Utf8Path::new(&bl_src).exists() {
            if let Err(e) =
                fs::copy(&bl_src, format!("{SP_EMU_ZONE_DIR}/bootleby.zip"))
            {
                warn(format!("copy bootleby from {bl_src}: {e}"));
            } else {
                bootleby = true;
            }
        }
    }
    // Flash each SP a per-instance state dir from its archive. sp-emu is an
    // illumos userland process, so run it here in the sled global zone, writing
    // the zone-visible state path the in-zone service reads at runtime.
    for sp in &fleet {
        let state = format!("{SP_EMU_ZONE_DIR}/state/{}", sp.port);
        if let Err(e) = fs::create_dir_all(&state) {
            warn(format!("mkdir {state}: {e}"));
            continue;
        }
        let archive = format!("{SP_EMU_ZONE_DIR}/{}.archive", sp.board);
        if !run_env(
            &bin_to,
            &["flash", "a", &archive],
            &[("SP_EMU_STATE_DIR", &state)],
        ) {
            warn(format!("sp-emu flash failed for port {}", sp.port));
        }
    }
    if let Err(e) =
        fs::write(SP_EMU_MANIFEST, sp_emu_manifest(&fleet, rot, bootleby))
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
    let ports: Vec<u16> = fleet.iter().map(|s| s.port).collect();
    note(format!(
        "sp-emu fleet up ({} SP(s): {ports:?}); sp-sim disabled",
        ports.len()
    ));
}

/// SMF manifest for the emulated SP fleet: one `svc:/oxide/voxel-sp-emu:sp<port>`
/// instance per SP, running `sp-emu run a 0` in the foreground so startd's
/// contract owns it (restart-on-crash, reboot-safe). Board, bridge, per-instance
/// state dir, and VPD identity go through the method environment. With `rot` each
/// SP runs oxide-rot-1 in-process over sprot from the shared rot image; there is
/// no separate RoT service.
fn sp_emu_manifest(fleet: &[EmuSp], rot: bool, bootleby: bool) -> String {
    let mut s = indoc! {r#"
        <?xml version="1.0"?>
        <!DOCTYPE service_bundle SYSTEM "/usr/share/lib/xml/dtd/service_bundle.dtd.1">
        <service_bundle type="manifest" name="voxel-sp-emu">
        <service name="oxide/voxel-sp-emu" type="service" version="1">
          <dependency name="multi_user" grouping="require_all" restart_on="none" type="service">
            <service_fmri value="svc:/milestone/multi-user:default"/>
          </dependency>
    "#}
    .to_string();
    for sp in fleet {
        let EmuSp { port, board, serial, part } = sp;
        s.push_str(&formatdoc! {r#"
            <instance name="sp{port}" enabled="true">
              <exec_method type="method" name="start" exec="{SP_EMU_IN_ZONE}/sp-emu run a 0" timeout_seconds="0">
                <method_context>
                  <method_environment>
                    <envvar name="SP_EMU_STATE_DIR" value="{SP_EMU_IN_ZONE}/state/{port}"/>
                    <envvar name="SP_EMU_BOARD" value="{board}"/>
                    <envvar name="SP_EMU_BRIDGE" value="[::1]:{port}"/>
                    <envvar name="SP_EMU_VPD_SERIAL" value="{serial}"/>
                    <envvar name="SP_EMU_NO_DEBUG" value="1"/>
        "#});
        // The sidecar carries no part number (stored as "-" in the manifest).
        if part.as_str() != "-" {
            s.push_str(&format!(
                "<envvar name=\"SP_EMU_VPD_PART\" value=\"{part}\"/>\n"
            ));
        }
        // In-process RoT over sprot; bootleby runs secure boot when staged.
        if rot {
            s.push_str(&formatdoc! {r#"
                <envvar name="SP_EMU_ROT_FLASH" value="{SP_EMU_IN_ZONE}/rot.image"/>
            "#});
            if bootleby {
                s.push_str(&format!(
                    "<envvar name=\"SP_EMU_ROT_BOOTLEBY\" value=\"{SP_EMU_IN_ZONE}/bootleby.zip\"/>\n"
                ));
            } else {
                s.push_str(
                    "<envvar name=\"SP_EMU_ROT_NO_BOOTLEBY\" value=\"1\"/>\n",
                );
            }
        }
        s.push_str(indoc! {r#"
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

    fn emu(port: u16, board: &str, serial: &str, part: &str) -> EmuSp {
        EmuSp {
            port,
            board: board.to_string(),
            serial: serial.to_string(),
            part: part.to_string(),
        }
    }

    #[test]
    fn sp_emu_manifest_structure_is_balanced() {
        let fleet = [
            emu(33300, "sidecar", "SimSidecar0", "-"),
            emu(33310, "gimlet", "2FAKE000", "913-0000019"),
        ];
        let m = sp_emu_manifest(&fleet, true, true);
        assert_eq!(m.matches("<instance name=\"sp").count(), 2);
        // In-process RoT: no separate rot service or instances.
        assert!(!m.contains("voxel-rot-emu"));
        assert_eq!(m.matches("<instance name=\"rot").count(), 0);
        assert_eq!(
            m.matches("<service ").count(),
            m.matches("</service>").count()
        );
        assert_eq!(
            m.matches("<instance ").count(),
            m.matches("</instance>").count()
        );
        assert!(m.contains("SP_EMU_ROT_FLASH"));
        assert!(m.contains("SP_EMU_ROT_BOOTLEBY"));
        assert!(!m.contains("SP_EMU_ROT_NO_BOOTLEBY"));
        assert!(m.contains("SP_EMU_VPD_SERIAL\" value=\"2FAKE000\""));
        assert!(m.contains("SP_EMU_VPD_PART\" value=\"913-0000019\""));
        // The sidecar has no part number, so no VPD_PART for it.
        assert!(m.contains("SP_EMU_BOARD\" value=\"sidecar\""));

        let no_bl = sp_emu_manifest(&fleet, true, false);
        assert!(no_bl.contains("SP_EMU_ROT_NO_BOOTLEBY"));
        assert!(!no_bl.contains("SP_EMU_ROT_BOOTLEBY"));

        let plain = sp_emu_manifest(
            &[emu(33310, "gimlet", "2FAKE000", "913-0000019")],
            false,
            false,
        );
        assert!(!plain.contains("SP_EMU_ROT_FLASH"));
        assert!(plain.contains("SP_EMU_BOARD\" value=\"gimlet\""));
    }
}
