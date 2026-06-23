//! `voxel sp` - manage and operate the rack's service processors.
//!
//! Two axes:
//!  - **build/stage** the `sp-emu` artifacts (`ready`/`flash`/`build`) - voxel's
//!    own emulator lifecycle; `pilot` has no equivalent.
//!  - **operate** the live SPs over MGS, `pilot sp`-style (`ls`/`state`/`exec`),
//!    by running `faux-mgs` (the same client `pilot` shells out to) inside the
//!    switch zone against each SP's loopback MGS port. Needs a running `--emu`
//!    rack. `exec` is the raw passthrough that unlocks the full faux-mgs surface
//!    (`inventory`, `component-details`, `read-sensor-value`, `dump`,
//!    `read-caboose`, `power-state`, `rot-boot-info`, ...).

use anyhow::anyhow;
use std::path::{Path, PathBuf};
use std::time::Duration;
use voxel_config::sp::{Sp, SpBackend, SpFleet, SpRole};
use voxel_config::VoxelConfig;

use crate::access::resolve_switch;
use crate::net::{node_external_ip, scp_to, ssh_capture};
use crate::topo::{build_topo, Topo};
use crate::SpCmd;

/// In-zone path we run faux-mgs from (also where we stage it on demand).
const FAUX_ZONE: &str = "/var/tmp/faux-mgs";
/// GZ-visible view of that path (the switch zone's root is mounted here).
const FAUX_GZ: &str = "/zone/oxz_switch/root/var/tmp/faux-mgs";
/// Pre-boot cargo-bay copy (staged by `topo::stage_sp_emu` when `[sp].faux_mgs`).
const FAUX_CARGO: &str = "/opt/cargo-bay/sp-emu/faux-mgs";
/// Baked-into-the-image copy (install-cp.sh, the self-contained path - present
/// even when neither `[sp].faux_mgs` nor `[sp].emu_bin` is configured at launch).
const FAUX_BAKED: &str = "/opt/oxide/sp-emu/faux-mgs";

pub(crate) async fn cmd_sp(cfg: &VoxelConfig, name: &str, cmd: &SpCmd) -> anyhow::Result<()> {
    match cmd {
        SpCmd::Ready => {
            ready(cfg);
            Ok(())
        }
        SpCmd::Flash { image, out } => flash(cfg, image, out),
        SpCmd::Build { commit } => build(commit),
        SpCmd::Ls { switch } => sp_ls(cfg, name, switch).await,
        SpCmd::Info { target, switch } => {
            print!("{}", sp_faux(cfg, name, switch, target, &["state"]).await?);
            Ok(())
        }
        SpCmd::Status { target, switch } => {
            print!("{}", sp_faux(cfg, name, switch, target, &["power-state"]).await?);
            Ok(())
        }
        SpCmd::Nmi { target, switch } => {
            print!("{}", sp_faux(cfg, name, switch, target, &["send-host-nmi"]).await?);
            Ok(())
        }
        SpCmd::Exec { target, switch, command } => {
            let parts: Vec<&str> = command.split_whitespace().collect();
            print!("{}", sp_faux(cfg, name, switch, target, &parts).await?);
            Ok(())
        }
    }
}

// --- operator commands (faux-mgs in the switch zone) -----------------------

/// Build the rack's SP fleet (the port map) for the rack `switch` lives in, and
/// return the scrimlet node whose switch zone we drive faux-mgs from.
fn switch_fleet(topo: &Topo, switch: &str) -> anyhow::Result<(SpFleet, libfalcon::NodeRef, String)> {
    let (s, n) = resolve_switch(topo, switch)?;
    let indices: Vec<usize> =
        topo.sleds.iter().filter(|(d, _)| d.rack == s.rack).map(|(d, _)| d.index).collect();
    // The backend doesn't affect the port map; this is an --emu rack.
    let fleet = SpFleet::for_gimlets(&indices, SpBackend::Emu);
    Ok((fleet, *n, s.name.clone()))
}

/// The MGS loopback port for an SP target. Accepts (in order): a node selector
/// (`sidecar` | `g0` | `g1` ...), a board serial (e.g. `BRM44220001`), or a raw
/// sim address (`[::1]:33310` | `33310`).
fn resolve_port(fleet: &SpFleet, target: &str) -> anyhow::Result<u16> {
    if let Some(sp) = fleet.sps.iter().find(|sp| sp.selector() == target) {
        return Ok(sp.base_port);
    }
    if let Some(sp) = fleet.sps.iter().find(|sp| sp_serial(sp) == target) {
        return Ok(sp.base_port);
    }
    if let Some(p) = target.rsplit(':').next().and_then(|s| s.parse::<u16>().ok()) {
        return Ok(p);
    }
    Err(anyhow!(
        "unknown SP target {target:?}: expected a serial (e.g. BRM44220001), a node \
         (sidecar | g0 | g1 ...), or a sim addr ([::1]:33310 | 33310)"
    ))
}

/// The board serial an emu SP reports, mirroring sp-emu's `build_vpd_eeprom`:
/// the sidecar is fixed; gimlets are port-derived `BRM4422000<idx>` where
/// idx = (base_port - 33300) / 10.
fn sp_serial(sp: &Sp) -> String {
    match sp.role {
        SpRole::Sidecar => "BRM42220001".to_string(),
        SpRole::Gimlet(_) => format!("BRM4422000{}", (sp.base_port - 33300) / 10),
    }
}

/// Make sure faux-mgs is present in the switch zone; copy it from the pre-staged
/// cargo-bay binary if not. `ip` is the scrimlet's host-LAN address (we drive the
/// zone over ssh, since `runner.exec` + `zlogin` doesn't terminate). Errors point
/// at `[sp].faux_mgs` when it can't be found.
fn ensure_faux(ip: &str, host_faux: Option<&str>) -> anyhow::Result<()> {
    let present = ssh_capture(ip, &format!("test -x {FAUX_GZ} && echo present"))
        .map(|o| o.contains("present"))
        .unwrap_or(false);
    if present {
        return Ok(());
    }
    // Preferred: scp it straight from the configured host binary - the proven path,
    // independent of in-zone 9p visibility of the cargo-bay copy.
    if let Some(faux) = host_faux {
        if Path::new(faux).exists()
            && scp_to(ip, faux, FAUX_GZ)
            && ssh_capture(ip, &format!("chmod +x {FAUX_GZ} && echo ok"))
                .map(|o| o.contains("ok"))
                .unwrap_or(false)
        {
            return Ok(());
        }
    }
    // Fallback: copy from a scrimlet-local binary - the baked image copy (the
    // self-contained path) or the pre-staged cargo-bay copy (if 9p exposes it).
    // Both live in the scrimlet GZ, so a single `cp` into the zone's /var/tmp
    // (FAUX_GZ) works without re-scp from the box.
    let staged = ssh_capture(
        ip,
        &format!(
            "for s in {FAUX_BAKED} {FAUX_CARGO}; do \
               if test -x $s; then cp $s {FAUX_GZ} && chmod +x {FAUX_GZ} && echo staged && break; fi; \
             done"
        ),
    )
    .map(|o| o.contains("staged"))
    .unwrap_or(false);
    if staged {
        Ok(())
    } else {
        Err(anyhow!(
            "couldn't get faux-mgs into the switch zone. Set `[sp].faux_mgs` to the faux-mgs \
             binary (currently {}) - `voxel sp` scps it in, or relaunch `--emu` to stage it.",
            host_faux.unwrap_or("unset")
        ))
    }
}

/// Run a faux-mgs command against one SP (by port) inside the switch zone over
/// ssh, returning its combined output. The emulator can drop the first request
/// under load, so retry (callers pick how hard).
fn faux_on(ip: &str, port: u16, args: &[&str], attempts: u32, timeout_ms: u32) -> anyhow::Result<String> {
    let remote = format!(
        "zlogin oxz_switch {FAUX_ZONE} --sp-sim-addr [::1]:{port} \
         --max-attempts {attempts} --per-attempt-timeout-millis {timeout_ms} {} 2>&1",
        args.join(" ")
    );
    ssh_capture(ip, &remote)
        .ok_or_else(|| anyhow!("faux-mgs over ssh failed (SP port {port}) - is the rack up?"))
}

/// The scrimlet host-LAN IP whose switch zone we drive faux-mgs in. Cached per
/// scrimlet so repeated `sp` calls skip the serial-console IP lookup (the console
/// wedges under RSS/console load; ssh to the cached IP is unaffected). The first
/// lookup is bounded so a wedged console fails fast instead of hanging; a stale
/// cache (after a relaunch) is cleared on the next ssh miss by the callers.
async fn switch_ip(topo: &Topo, switch: &str) -> anyhow::Result<(SpFleet, String, String)> {
    let (fleet, node, sw) = switch_fleet(topo, switch)?;
    if let Some(ip) = read_cached_ip(&sw) {
        return Ok((fleet, ip, sw));
    }
    let ip = tokio::time::timeout(Duration::from_secs(15), node_external_ip(&topo.runner, node, false))
        .await
        .map_err(|_| {
            anyhow!(
                "timed out resolving {sw}'s IP over the serial console (it can wedge under \
                 console/RSS load). Retry shortly, or once `voxel host ls` works."
            )
        })?
        .map_err(|e| anyhow!("{e} - is the rack up? (`voxel status`)"))?;
    write_cached_ip(&sw, &ip);
    Ok((fleet, ip, sw))
}

/// Per-scrimlet host-LAN IP cache (workdir-relative; voxel anchors to the project
/// root). Lets `sp` commands reach the switch zone over ssh without re-touching
/// the serial console after the first resolve.
fn ip_cache(node: &str) -> PathBuf {
    PathBuf::from(".falcon").join(format!(".sp-ip-{node}"))
}
fn read_cached_ip(node: &str) -> Option<String> {
    std::fs::read_to_string(ip_cache(node))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}
fn write_cached_ip(node: &str, ip: &str) {
    let _ = std::fs::write(ip_cache(node), ip);
}
fn clear_cached_ip(node: &str) {
    let _ = std::fs::remove_file(ip_cache(node));
}

/// `sp <verb...>` against a named target: resolve switch + port, stage faux-mgs,
/// run it (generous retries - the caller wants the answer).
async fn sp_faux(
    cfg: &VoxelConfig,
    name: &str,
    switch: &str,
    target: &str,
    args: &[&str],
) -> anyhow::Result<String> {
    let topo = build_topo(cfg, name)?;
    let (fleet, ip, sw) = switch_ip(&topo, switch).await?;
    let port = resolve_port(&fleet, target)?;
    // An ssh miss usually means a stale cached IP (rack was relaunched) - drop it
    // so the next call re-resolves.
    if let Err(e) = ensure_faux(&ip, cfg.sp.faux_mgs.as_deref()) {
        clear_cached_ip(&sw);
        return Err(e);
    }
    faux_on(&ip, port, args, 5, 15000).map_err(|e| {
        clear_cached_ip(&sw);
        e
    })
}

/// `voxel sp ls` - enumerate every SP via the switch zone, pilot-style table.
async fn sp_ls(cfg: &VoxelConfig, name: &str, switch: &str) -> anyhow::Result<()> {
    let topo = build_topo(cfg, name)?;
    let (fleet, ip, sw) = switch_ip(&topo, switch).await?;
    if let Err(e) = ensure_faux(&ip, cfg.sp.faux_mgs.as_deref()) {
        clear_cached_ip(&sw);
        return Err(e);
    }
    println!("SPs via {sw} (oxz_switch, [::1]):");
    println!(
        "{:<8}  {:<5}  {:<8}  {:<12}  {:<6}  {}",
        "SP", "PORT", "TYPE", "SERIAL", "POWER", "ARCHIVE"
    );
    // Probe every SP in ONE ssh: a single zlogin runs faux-mgs for each port
    // in-zone, back to back. Doing 5 separate ssh+zlogin calls (sequential OR
    // parallel) is the slow/variable path - zone login serializes, so concurrency
    // just trades steady ~1.2s for frequent multi-second spikes. A warm SP answers
    // each probe in ~20ms, so the whole table is one round trip (~0.3s). Outputs
    // are split back out by the `@@SP <port>` markers, in `fleet.sps` order.
    // One ssh+zlogin, but launch a faux-mgs per SP CONCURRENTLY in the zone (each
    // to its own temp file), then wait and emit per-port. Probing the 5 SPs
    // sequentially summed each probe's discovery to ~1.5s under box load; running
    // them concurrently makes the table ~one probe deep (~0.4s). We pass the
    // script directly to zlogin (NOT via `sh -c`, whose quoting zlogin strips),
    // single-quoted so g0 hands it over as one arg; the zone shell parses it.
    // faux-mgs's slog INFO goes to stderr - drop it so `field()` sees clean stdout.
    let ports: Vec<u16> = fleet.sps.iter().map(|s| s.base_port).collect();
    let plist: String = ports.iter().map(|p| format!(" {p}")).collect();
    let probe = format!(
        "for p in{plist}; do {FAUX_ZONE} --sp-sim-addr [::1]:$p --max-attempts 3 \
         --per-attempt-timeout-millis 8000 state >/var/tmp/spls.$p 2>/dev/null & done; wait; \
         for p in{plist}; do echo \"@@SP $p\"; cat /var/tmp/spls.$p; rm -f /var/tmp/spls.$p; done"
    );
    let combined =
        ssh_capture(&ip, &format!("zlogin oxz_switch '{probe}'")).unwrap_or_default();
    let outputs: Vec<String> = {
        let mut v = vec![String::new(); ports.len()];
        let mut idx: Option<usize> = None;
        for line in combined.lines() {
            if let Some(rest) = line.strip_prefix("@@SP ") {
                idx = rest.trim().parse::<u16>().ok().and_then(|p| ports.iter().position(|&q| q == p));
            } else if let Some(i) = idx {
                v[i].push_str(line);
                v[i].push('\n');
            }
        }
        v
    };
    let mut answered = false;
    for (sp, out) in fleet.sps.iter().zip(outputs) {
        let typ = match sp.role {
            SpRole::Sidecar => "sidecar",
            SpRole::Gimlet(_) => "gimlet",
        };
        // An SP that answered has a hubris archive / power even if its serial is
        // blank (the emu gimlets carry no VPD serial yet) - key "answered" off
        // that, not serial.
        let archive = field(&out, "hubris archive:");
        let power = field(&out, "power state:");
        let responded = !archive.is_empty() || !power.is_empty();
        if responded {
            answered = true;
        }
        let serial = field(&out, "serial number:");
        let serial = if !responded {
            "(no answer)".to_string()
        } else if serial.is_empty() {
            "-".to_string()
        } else {
            serial
        };
        println!("{:<8}  {:<5}  {:<8}  {:<12}  {:<6}  {}", sp.selector(), sp.base_port, typ, serial, power, archive);
    }
    if !answered {
        // Either a stale cached IP or the SPs are wedged - drop the cache so a
        // retry re-resolves, and tell the operator.
        clear_cached_ip(&sw);
        eprintln!(
            "[voxel] no SP answered - the rack may be mid-bring-up, the SPs busy under MGS \
             load (retry), or the cached IP was stale (now cleared; retry)."
        );
    }
    Ok(())
}

/// Pull `label: value` out of faux-mgs text output (first match), trimmed.
fn field(out: &str, label: &str) -> String {
    out.lines()
        .find_map(|l| l.trim().strip_prefix(label))
        .map(|v| v.trim().to_string())
        .unwrap_or_default()
}

// --- artifact commands (was `Ls`, now `Ready`; flash/build unchanged) -------

fn present(p: &str) -> bool {
    Path::new(p).exists()
}

fn show(name: &str, val: Option<&str>) {
    match val {
        Some(p) => {
            println!("  {name:<14} {p}  [{}]", if present(p) { "present" } else { "MISSING" })
        }
        None => println!("  {name:<14} (unset)"),
    }
}

fn ready(cfg: &VoxelConfig) {
    let sp = &cfg.sp;
    println!("sp-emu artifacts ([sp] in voxel.toml):");
    show("emu_bin", sp.emu_bin.as_deref());
    show("sidecar_image", sp.sidecar_image.as_deref());
    show("gimlet_image", sp.gimlet_image.as_deref());
    show("faux_mgs", sp.faux_mgs.as_deref());
    let ready = [&sp.emu_bin, &sp.sidecar_image, &sp.gimlet_image]
        .iter()
        .all(|v| v.as_deref().map(present).unwrap_or(false));
    println!(
        "\n`voxel launch --emu` ready: {}",
        if ready { "yes" } else { "no - set/fix the paths above (or `voxel sp build`)" }
    );
}

fn flash(cfg: &VoxelConfig, image: &Path, out: &Path) -> anyhow::Result<()> {
    let emu_bin = cfg
        .sp
        .emu_bin
        .as_deref()
        .ok_or_else(|| anyhow!("[sp].emu_bin is not set (path to the sp-emu binary)"))?;
    if !image.exists() {
        return Err(anyhow!("image not found: {}", image.display()));
    }
    eprintln!("[voxel] flashing {} -> {}", image.display(), out.display());
    let status = std::process::Command::new(emu_bin)
        .env("SP_EMU_FLASH", out)
        .args(["flash", "a"])
        .arg(image)
        .status()
        .map_err(|e| anyhow!("run {emu_bin}: {e}"))?;
    if !status.success() {
        return Err(anyhow!("sp-emu flash failed"));
    }
    println!("flashed {}", out.display());
    Ok(())
}

fn build(commit: &str) -> anyhow::Result<()> {
    let script = build_sp_script()?;
    eprintln!("[voxel] building sp-emu hubris images for {commit} via {}", script.display());
    let status = std::process::Command::new("bash")
        .arg(&script)
        .arg(commit)
        .status()
        .map_err(|e| anyhow!("run {}: {e}", script.display()))?;
    if !status.success() {
        return Err(anyhow!("build-sp.sh failed for {commit}"));
    }
    Ok(())
}

/// Locate `voxel-image/build-sp.sh` (mirrors `image::build_cp_script`).
fn build_sp_script() -> anyhow::Result<PathBuf> {
    if let Ok(p) = std::env::var("VOXEL_BUILD_SP") {
        return Ok(PathBuf::from(p));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let cand = dir.join("../../voxel-image/build-sp.sh");
            if cand.exists() {
                return Ok(cand);
            }
        }
    }
    let cwd = PathBuf::from("voxel-image/build-sp.sh");
    if cwd.exists() {
        return Ok(cwd);
    }
    Err(anyhow!("can't find build-sp.sh - set VOXEL_BUILD_SP to its path"))
}
