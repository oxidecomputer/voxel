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
use voxel_config::VoxelConfig;
use voxel_config::sp::{PORT_STRIDE, SP_PORT_BASE, Sp, SpBackend, SpFleet, SpRole};

use crate::SpCmd;
use crate::access::resolve_switch;
use crate::net::{
    SWITCH_ZONE_ROOT, node_external_ip, scp_from, scp_to, ssh_capture, ssh_output, zlogin,
};
use crate::topo::{GIMLET_SERIAL_PREFIX, Topo, build_topo};
use crate::util::shell_quote;

/// In-zone path we run faux-mgs from (also where we stage it on demand). The
/// GZ-visible view is `SWITCH_ZONE_ROOT + FAUX_ZONE` (see `ensure_faux`).
const FAUX_ZONE: &str = "/var/tmp/faux-mgs";
/// Pre-boot cargo-bay copy (staged by `topo::stage_sp_emu` when `[sp].faux_mgs`).
const FAUX_CARGO: &str = "/opt/cargo-bay/sp-emu/faux-mgs";
/// Baked-into-the-image copy (install-cp.sh, the self-contained path - present
/// even when neither `[sp].faux_mgs` nor `[sp].emu_bin` is configured at launch).
const FAUX_BAKED: &str = "/opt/oxide/sp-emu/faux-mgs";
/// The baked sp-emu fleet dir, in-zone and (from the sled GZ) GZ-visible. The
/// per-SP flash files (`<port>.flash`), the shared RoT flash (`rot.flash`), and
/// the `sp-emu` binary all live here; `voxel-init` starts one
/// `svc:/oxide/voxel-sp-emu:sp<port>` (and `voxel-rot-emu:rot<port>`) per SP off
/// these. `reflash` swaps a flash + restarts the matching service.
const SP_EMU_ZONE: &str = "/opt/oxide/sp-emu";

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
            print!(
                "{}",
                sp_faux(cfg, name, switch, target, &["power-state"]).await?
            );
            Ok(())
        }
        SpCmd::Nmi { target, switch } => {
            print!(
                "{}",
                sp_faux(cfg, name, switch, target, &["send-host-nmi"]).await?
            );
            Ok(())
        }
        SpCmd::Exec {
            target,
            switch,
            command,
        } => {
            // `command` is the passthrough token(s) after `-e`. Split each on
            // whitespace so a single quoted string (`-e "read-caboose 0"`) and
            // separate args (`-e read-caboose 0`) both flatten to faux-mgs argv.
            let parts: Vec<&str> = command.iter().flat_map(|s| s.split_whitespace()).collect();
            print!("{}", sp_faux(cfg, name, switch, target, &parts).await?);
            Ok(())
        }
        SpCmd::Reflash {
            target,
            image,
            switch,
        } => sp_reflash(cfg, name, switch, target, image).await,
        SpCmd::Debug {
            target,
            off,
            switch,
        } => sp_debug(cfg, name, switch, target, *off).await,
        SpCmd::Dump {
            target,
            ringbuf,
            switch,
        } => sp_dump(cfg, name, switch, target, *ringbuf).await,
        SpCmd::Ipcc {
            target,
            cmd,
            switch,
        } => sp_ipcc(cfg, name, switch, target, cmd).await,
    }
}

// --- operator commands (faux-mgs in the switch zone) -----------------------

/// Build the rack's SP fleet (the port map) for the rack `switch` lives in, and
/// return the scrimlet node whose switch zone we drive faux-mgs from.
fn switch_fleet(
    topo: &Topo,
    switch: &str,
) -> anyhow::Result<(SpFleet, libfalcon::NodeRef, String)> {
    let (s, n) = resolve_switch(topo, switch)?;
    let indices: Vec<usize> = topo
        .sleds
        .iter()
        .filter(|(d, _)| d.rack == s.rack)
        .map(|(d, _)| d.index)
        .collect();
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
    if let Some(p) = target
        .rsplit(':')
        .next()
        .and_then(|s| s.parse::<u16>().ok())
    {
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
        SpRole::Gimlet(_) => {
            format!(
                "{GIMLET_SERIAL_PREFIX}{}",
                (sp.base_port - SP_PORT_BASE) / PORT_STRIDE
            )
        }
    }
}

/// Make sure faux-mgs is present in the switch zone; copy it from the pre-staged
/// cargo-bay binary if not. `ip` is the scrimlet's host-LAN address (we drive the
/// zone over ssh, since `runner.exec` + `zlogin` doesn't terminate). Errors point
/// at `[sp].faux_mgs` when it can't be found.
fn ensure_faux(ip: &str, host_faux: Option<&str>) -> anyhow::Result<()> {
    // GZ-visible view of the in-zone faux-mgs path.
    let faux_gz = format!("{SWITCH_ZONE_ROOT}{FAUX_ZONE}");
    let present = ssh_capture(ip, &format!("test -x {faux_gz} && echo present"))
        .map(|o| o.contains("present"))
        .unwrap_or(false);
    if present {
        return Ok(());
    }
    // Preferred: scp it straight from the configured host binary - the proven path,
    // independent of in-zone 9p visibility of the cargo-bay copy.
    if let Some(faux) = host_faux {
        if Path::new(faux).exists()
            && scp_to(ip, faux, &faux_gz)
            && ssh_capture(ip, &format!("chmod +x {faux_gz} && echo ok"))
                .map(|o| o.contains("ok"))
                .unwrap_or(false)
        {
            return Ok(());
        }
    }
    // Fallback: copy from a scrimlet-local binary - the baked image copy (the
    // self-contained path) or the pre-staged cargo-bay copy (if 9p exposes it).
    // Both live in the scrimlet GZ, so a single `cp` into the zone's /var/tmp
    // (faux_gz) works without re-scp from the box.
    let staged = ssh_capture(
        ip,
        &format!(
            "for s in {FAUX_BAKED} {FAUX_CARGO}; do \
               if test -x $s; then cp $s {faux_gz} && chmod +x {faux_gz} && echo staged && break; fi; \
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
/// under load, so retry (callers pick how hard). Uses [`ssh_output`] (not
/// `ssh_capture`) so a non-zero faux-mgs exit returns the SP's OWN error text
/// (e.g. "the image caboose does not contain 'GITC'", "code: Unconfigured") - a
/// bad arg or an empty slot is the SP answering, not the rack being down. Only a
/// genuine ssh transport failure (None) maps to "is the switch zone reachable".
fn faux_on(
    ip: &str,
    port: u16,
    args: &[&str],
    attempts: u32,
    timeout_ms: u32,
) -> anyhow::Result<String> {
    let remote = zlogin(&format!(
        "{FAUX_ZONE} --sp-sim-addr [::1]:{port} \
         --max-attempts {attempts} --per-attempt-timeout-millis {timeout_ms} {} 2>&1",
        args.join(" ")
    ));
    ssh_output(ip, &remote)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| {
            anyhow!(
                "couldn't reach faux-mgs in the switch zone (SP port {port}) - is the switch zone \
             reachable? (`voxel status`)"
            )
        })
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
    let ip = tokio::time::timeout(
        Duration::from_secs(15),
        node_external_ip(&topo.runner, node, false),
    )
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

/// `voxel sp reflash <target> <image>` - re-flash a live SP (or the shared RoT)
/// and restart its sp-emu service: the firmware counterpart to `voxel rack
/// patch`. Flashes in-zone with the BAKED `sp-emu` (so it works on a
/// self-contained image with no `[sp]` paths set), then verifies over MGS. Live
/// + ephemeral - a clean relaunch reverts to the image; bake it via `build-cp.sh`
/// to persist. `target == "rot"` swaps the shared raw `rot.flash` (every RoT
/// bridge serves it) and restarts them all; otherwise it's a single SP.
async fn sp_reflash(
    cfg: &VoxelConfig,
    name: &str,
    switch: &str,
    target: &str,
    image: &Path,
) -> anyhow::Result<()> {
    if !image.exists() {
        return Err(anyhow!("image not found: {}", image.display()));
    }
    let local = image
        .to_str()
        .ok_or_else(|| anyhow!("non-utf8 image path"))?;
    let topo = build_topo(cfg, name)?;
    let (fleet, ip, sw) = switch_ip(&topo, switch).await?;
    // The baked sp-emu fleet must be present - reflash is meaningless on a
    // sp-sim rack (there's no flash file / voxel-sp-emu service to swap).
    let have = ssh_capture(&ip, &format!("test -x {SP_EMU_ZONE}/sp-emu && echo ok"))
        .map(|o| o.contains("ok"))
        .unwrap_or(false);
    if !have {
        clear_cached_ip(&sw);
        return Err(anyhow!(
            "no baked sp-emu in {sw}:{SP_EMU_ZONE} - reflash needs a running --emu / --emu-rot rack"
        ));
    }

    if target == "rot" {
        // rot.flash is a raw oxide-rot-1 image shared by every voxel-rot-emu
        // instance (build-cp.sh copies `[sp].rot_image` -> rot.flash). Replace it
        // and restart them all; the SPs reconnect to the bridge.
        eprintln!(
            "[voxel] reflashing shared RoT (rot.flash) on {sw} from {}",
            image.display()
        );
        if !scp_to(
            &ip,
            local,
            &format!("{SWITCH_ZONE_ROOT}{SP_EMU_ZONE}/rot.flash"),
        ) {
            clear_cached_ip(&sw);
            return Err(anyhow!("scp of RoT image into {sw} failed"));
        }
        // grep on one word ("voxel-rot-emu") is the nested-ssh-safe way to list
        // the instances (alternation/brackets silently fail through ssh).
        let restart = "n=0; for f in $(svcs -H -o fmri | grep voxel-rot-emu); do svcadm restart $f && n=$((n+1)); done; echo RESTARTED $n";
        let out =
            ssh_output(&ip, &zlogin(&format!("{} 2>&1", shell_quote(restart)))).unwrap_or_default();
        if !out.contains("RESTARTED") {
            return Err(anyhow!(
                "RoT image placed but restart failed on {sw}: {}",
                out.trim()
            ));
        }
        eprintln!("[voxel] {}; verifying via rot-boot-info ...", out.trim());
        match sp_faux(cfg, name, switch, "sidecar", &["rot-boot-info"]).await {
            Ok(o) => print!("{o}"),
            Err(e) => eprintln!("[voxel] RoT reflashed; rot-boot-info not yet available ({e})"),
        }
        return Ok(());
    }

    // SP reflash: flash the hubris zip into the target port's flash, swap, restart.
    let port = resolve_port(&fleet, target)?;
    let zip = image
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| anyhow!("bad image filename"))?;
    eprintln!(
        "[voxel] reflashing SP {target} (port {port}) on {sw} from {}",
        image.display()
    );
    let remote_zip_gz = format!("{SWITCH_ZONE_ROOT}/var/tmp/{zip}");
    if !scp_to(&ip, local, &remote_zip_gz) {
        clear_cached_ip(&sw);
        return Err(anyhow!("scp of hubris image into {sw} failed"));
    }
    // Flash in-zone into a temp file (the baked sp-emu does `flash a`), swap it
    // atomically over the live <port>.flash, then restart just that SP's service.
    let script = format!(
        "set -e; SP_EMU_FLASH=/var/tmp/reflash.{port} {SP_EMU_ZONE}/sp-emu flash a /var/tmp/{zip}; \
         mv /var/tmp/reflash.{port} {SP_EMU_ZONE}/{port}.flash; rm -f /var/tmp/{zip}; \
         svcadm restart svc:/oxide/voxel-sp-emu:sp{port}; echo REFLASH_OK"
    );
    let out =
        ssh_output(&ip, &zlogin(&format!("{} 2>&1", shell_quote(&script)))).unwrap_or_default();
    if !out.contains("REFLASH_OK") {
        return Err(anyhow!("SP reflash failed on {sw}: {}", out.trim()));
    }
    // The SP re-runs its ~340M-instruction preboot (~30s) before MGS answers;
    // sp_faux retries generously, so this waits out the boot and returns fresh state.
    eprintln!("[voxel] SP {target} reflashed + restarting; waiting for it to boot (~30s) ...");
    match sp_faux(cfg, name, switch, target, &["state"]).await {
        Ok(o) => print!("{o}"),
        Err(e) => eprintln!(
            "[voxel] reflashed, but SP {target} not responding yet ({e}); retry `voxel sp info {target}` shortly"
        ),
    }
    Ok(())
}

/// `voxel sp debug <target> [--off]` - toggle the in-zone humility debug
/// listeners for one SP by flipping `SP_EMU_NO_DEBUG` on its sp-emu service +
/// restarting it. The SPs already run in `gdb` mode; that env var is the only
/// thing suppressing the gdb/ocd listeners (`sp-emu` src/gdb.rs), so this is the
/// on/off switch. On enable, prints the per-SP humility ports + attach command.
/// Live + ephemeral (a clean relaunch reverts to baked production = debug off).
///
/// We edit the service's `start/environment` via an `svccfg -f` command file
/// (scp'd into the zone) - preserving the rest of the env, toggling just the one
/// var - then `svcadm refresh` + `restart`. Listeners bind after the SP's ~30s
/// preboot.
async fn sp_debug(
    cfg: &VoxelConfig,
    name: &str,
    switch: &str,
    target: &str,
    off: bool,
) -> anyhow::Result<()> {
    let topo = build_topo(cfg, name)?;
    let (fleet, ip, sw) = switch_ip(&topo, switch).await?;
    let port = resolve_port(&fleet, target)?;
    let fmri = format!("svc:/oxide/voxel-sp-emu:sp{port}");

    // Read the current method environment so we only toggle SP_EMU_NO_DEBUG and
    // keep everything else (board/flash/bridge/rot-service) intact.
    let env = ssh_capture(
        &ip,
        &zlogin(&format!("svcprop -p start/environment {fmri}")),
    )
    .map(|s| s.trim().to_string())
    .filter(|s| !s.is_empty())
    .ok_or_else(|| {
        clear_cached_ip(&sw);
        anyhow!("couldn't read {fmri} env on {sw} - is this a running --emu / --emu-rot rack?")
    })?;
    let mut tokens: Vec<String> = env
        .split_whitespace()
        .filter(|t| !t.starts_with("SP_EMU_NO_DEBUG"))
        .map(String::from)
        .collect();
    if off {
        tokens.push("SP_EMU_NO_DEBUG=1".to_string());
    }

    // svccfg command file: re-set the whole (toggled) env list. Each token is
    // double-quoted so values like `[::1]:33320` survive. Shipped as a FILE to
    // dodge nested ssh/zlogin quoting of the parens + quotes.
    let quoted: Vec<String> = tokens.iter().map(|t| format!("\"{t}\"")).collect();
    let content = format!(
        "select {fmri}\nsetprop start/environment = astring: ({})\n",
        quoted.join(" ")
    );
    let local = std::env::temp_dir().join(format!("voxel-sp-env-{port}.scfg"));
    std::fs::write(&local, &content).map_err(|e| anyhow!("write {}: {e}", local.display()))?;
    let remote_gz = format!("{SWITCH_ZONE_ROOT}/var/tmp/voxel-sp-env-{port}.scfg");
    let remote = format!("/var/tmp/voxel-sp-env-{port}.scfg");
    if !scp_to(&ip, local.to_str().unwrap_or_default(), &remote_gz) {
        clear_cached_ip(&sw);
        return Err(anyhow!("scp of the svccfg file into {sw} failed"));
    }
    let apply = zlogin(&format!(
        "'svccfg -f {remote} && svcadm refresh {fmri} && svcadm restart {fmri} && rm -f {remote} && echo APPLIED_OK'"
    ));
    let out = ssh_output(&ip, &format!("{apply} 2>&1")).unwrap_or_default();
    if !out.contains("APPLIED_OK") {
        return Err(anyhow!(
            "failed to apply debug toggle on {sw}: {}",
            out.trim()
        ));
    }

    if off {
        eprintln!(
            "[voxel] {target} (port {port}) debug DISABLED on {sw}; sp-emu restarting (listeners off, production mode)"
        );
        return Ok(());
    }
    // Per-SP ports: offset by the bridge port (sp-emu src/gdb.rs). gdb=3333+off,
    // ocd=6666+off, off = base_port - 33300. humility's transports (verified
    // against this humility build): `-p ocdgdb` (read-only, GDB-RSP) honors
    // HUMILITY_OCD_PORT -> works on ANY port; `-p ocd` (read+write, OpenOCD-Tcl)
    // has a HARDCODED port 6666 -> only reaches the SP on bridge 33300.
    let o = port.wrapping_sub(SP_PORT_BASE);
    let (gdb, ocd) = (3333 + o, 6666 + o);
    eprintln!(
        "[voxel] {target} (port {port}) debug ENABLED on {sw}; sp-emu restarting (listeners ready in ~30s after preboot)"
    );
    println!(
        "humility attach (listeners are 127.0.0.1 inside oxz_switch on {sw} - run humility there, or tunnel):"
    );
    println!("  reads (tasks/readmem/ringbuf/readvar) - GDB-RSP :{gdb}");
    println!("    ssh -L {gdb}:127.0.0.1:{gdb} root@{ip}");
    println!("    HUMILITY_OCD_PORT={gdb} humility -a <archive.zip> -p ocdgdb tasks");
    if ocd == 6666 {
        println!("  read+write (writemem/hiffy) - OpenOCD-Tcl :{ocd}");
        println!("    ssh -L {ocd}:127.0.0.1:{ocd} root@{ip}");
        println!("    humility -a <archive.zip> -p ocd <cmd>");
    } else {
        println!(
            "  read+write (-p ocd) needs port 6666 (hardcoded in humility) - only the sidecar (33300); this SP's ocd is :{ocd}"
        );
    }
    Ok(())
}

/// `voxel sp ipcc <target> [--cmd identity|bsu]` - drive one host<->SP exchange
/// over the SP's control UART (RFD 316 / IPCC). sp-emu models UART7 and exposes
/// it as `SP_EMU_HOST_UART` (a Unix socket it connects to). We stage the host
/// sp-emu (which has the `ipcc` host-role subcommand) into the zone as the probe,
/// start it listening, arm the SP with that socket + restart (so it connects at
/// boot), then the probe sends a `HostToSp` request and decodes the `SpToHost`
/// reply - proving the emulated SP speaks IPCC. Everything runs in-zone (the SP's
/// UART socket is in-zone loopback). Live + ephemeral (the arming reverts on a
/// clean relaunch). Emu-only.
async fn sp_ipcc(
    cfg: &VoxelConfig,
    name: &str,
    switch: &str,
    target: &str,
    command: &str,
) -> anyhow::Result<()> {
    if !matches!(
        command,
        "identity" | "bsu" | "macs" | "status" | "inventory"
    ) {
        return Err(anyhow!(
            "--cmd must be one of identity|bsu|macs|status|inventory (got `{command}`)"
        ));
    }
    let topo = build_topo(cfg, name)?;
    let (fleet, ip, sw) = switch_ip(&topo, switch).await?;
    let port = resolve_port(&fleet, target)?;
    // The probe is the host sp-emu binary (it carries the `ipcc` subcommand); the
    // baked in-zone sp-emu already handles SP_EMU_HOST_UART on the SP side.
    let emu_bin = cfg.sp.emu_bin.as_deref().ok_or_else(|| {
        anyhow!("[sp].emu_bin not set - need the sp-emu binary (with the `ipcc` subcommand)")
    })?;
    if !Path::new(emu_bin).exists() {
        return Err(anyhow!("sp-emu binary not found: {emu_bin}"));
    }
    if !scp_to(
        &ip,
        emu_bin,
        &format!("{SWITCH_ZONE_ROOT}/var/tmp/sp-emu-ipcc"),
    ) {
        clear_cached_ip(&sw);
        return Err(anyhow!("scp of sp-emu into {sw} failed"));
    }

    // Orchestration shipped as a FILE (avoids nested ssh/zlogin quoting). A
    // persistent broker (`ipcc-serve`) holds the SP's UART connection so repeats
    // skip the reboot: FAST path just asks the broker (`ipcc-req`); on a miss we
    // start the broker (if down) + arm the SP's UART7 socket + restart it so it
    // connects, then poll the broker for the reply. Only PORT + CMD vary.
    let script = format!(
        r#"set -u
PORT={port}
CMD={command}
FMRI=svc:/oxide/voxel-sp-emu:sp$PORT
SP=/var/tmp/ipcc-$PORT.sock
CTL=/var/tmp/ipcc-ctl-$PORT.sock
BIN=/var/tmp/sp-emu-ipcc
chmod +x "$BIN"
# Fast path: broker already up + holding the SP connection.
FIRST=$("$BIN" ipcc-req "$CTL" "$CMD" 2>/dev/null)
rc=$?
if [ "$rc" -eq 0 ]; then
  printf '%s\n' "$FIRST"
  exit 0
fi
# rc 3 = broker down -> start it (persistent); rc 4 = broker up, SP not connected.
if [ "$rc" -eq 3 ]; then
  nohup "$BIN" ipcc-serve "$SP" "$CTL" >/var/tmp/ipcc-broker-$PORT.log 2>&1 </dev/null &
  sleep 1
fi
# Arm the SP's UART7 with the broker socket + restart so it connects at boot.
cur=$(svcprop -p start/environment "$FMRI" 2>/dev/null)
q=""
for t in $(printf '%s\n' $cur | grep -v '^SP_EMU_HOST_UART=') "SP_EMU_HOST_UART=$SP"; do
  q="$q \"$t\""
done
printf 'select %s\nsetprop start/environment = astring: (%s )\n' "$FMRI" "$q" > /var/tmp/ipcc-env-$PORT.scfg
svccfg -f /var/tmp/ipcc-env-$PORT.scfg && svcadm refresh "$FMRI" && svcadm restart "$FMRI"
# Wait for the SP to boot + connect, then read the reply through the broker.
for i in $(seq 1 70); do
  R=$("$BIN" ipcc-req "$CTL" "$CMD" 2>/dev/null) && {{ printf '%s\n' "$R"; exit 0; }}
  sleep 1
done
echo "[voxel] IPCC timed out"
exit 1
"#
    );
    let local = std::env::temp_dir().join(format!("voxel-ipcc-{port}.sh"));
    std::fs::write(&local, &script).map_err(|e| anyhow!("write {}: {e}", local.display()))?;
    if !scp_to(
        &ip,
        local.to_str().unwrap_or_default(),
        &format!("{SWITCH_ZONE_ROOT}/var/tmp/voxel-ipcc-{port}.sh"),
    ) {
        clear_cached_ip(&sw);
        return Err(anyhow!("scp of the IPCC script into {sw} failed"));
    }
    eprintln!("[voxel] {target} (port {port}) on {sw}: ipcc {command}");
    let out = ssh_output(&ip, &zlogin(&format!("bash /var/tmp/voxel-ipcc-{port}.sh")))
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| {
            clear_cached_ip(&sw);
            anyhow!("couldn't run the IPCC probe in {sw}")
        })?;
    print!("{out}");
    if !out.contains("SpToHost reply") {
        return Err(anyhow!(
            "no decoded IPCC reply (the SP may still be booting - retry `voxel sp ipcc {target}`)"
        ));
    }
    Ok(())
}

/// `voxel sp dump <target> [--ringbuf]` - force + decode a crash dump of one live
/// emulated SP. sp-emu writes a humility-hydrate RAM snapshot on demand: when
/// `<SP_EMU_DUMP_DIR>/.trigger` appears it dumps RAM (flash comes from the archive)
/// and swaps in `.done` (src/mem.rs `write_hydrate_dump`). We arm the SP's service
/// with that dir + its archive id (a one-time ~30s restart if not already armed),
/// touch the trigger in-zone, pull the zip to the host, and run `humility hydrate`
/// + `tasks`/`ringbuf` against the SP's hubris archive - all where humility + the
/// archive live (the debug listeners are in-zone loopback, so a probe-based dump
/// would need a tunnel; this needs none). Live + ephemeral: the arming reverts on
/// a clean relaunch. Emu-only (sp-sim never faults / has no dump dir).
async fn sp_dump(
    cfg: &VoxelConfig,
    name: &str,
    switch: &str,
    target: &str,
    ringbuf: bool,
) -> anyhow::Result<()> {
    let topo = build_topo(cfg, name)?;
    let (fleet, ip, sw) = switch_ip(&topo, switch).await?;
    let port = resolve_port(&fleet, target)?;
    let selector = fleet
        .sps
        .iter()
        .find(|s| s.base_port == port)
        .map(|s| s.selector())
        .unwrap_or_else(|| target.to_string());

    // The hubris archive for this SP (host path): humility needs it to fill flash
    // (the dump omits flash) and its image id must match the dump's.
    let archive = cfg.sp.image_for(&selector).ok_or_else(|| {
        anyhow!(
            "no hubris archive for {selector} in [sp] (set sidecar_image / gimlet_image) - \
             is this an --emu rack?"
        )
    })?;
    if !Path::new(archive).exists() {
        return Err(anyhow!("hubris archive not found: {archive}"));
    }
    // humility runs on the HOST; don't bake a sibling-repo path - take it from
    // $VOXEL_HUMILITY, else `humility` on PATH.
    let humility = std::env::var("VOXEL_HUMILITY").unwrap_or_else(|_| "humility".to_string());

    // A baked sp-emu (emu rack) is required - sp-sim has no dump dir to arm.
    let have = ssh_capture(&ip, &format!("test -x {SP_EMU_ZONE}/sp-emu && echo ok"))
        .map(|o| o.contains("ok"))
        .unwrap_or(false);
    if !have {
        clear_cached_ip(&sw);
        return Err(anyhow!(
            "no baked sp-emu in {sw}:{SP_EMU_ZONE} - dump needs a running --emu / --emu-rot rack"
        ));
    }

    // The dump's archive id must equal the archive's image id (humility hydrate
    // rejects a mismatch), so read the SP's running archive id over MGS.
    let state = sp_faux(cfg, name, switch, target, &["state"]).await?;
    let archive_id = state
        .lines()
        .find_map(|l| l.split("hubris archive:").nth(1))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("couldn't read {target}'s hubris archive id from `sp state`"))?;

    let fmri = format!("svc:/oxide/voxel-sp-emu:sp{port}");
    let dump_dir = format!("/var/tmp/spdump-{port}");
    let want_dir = format!("SP_EMU_DUMP_DIR={dump_dir}");
    let want_id = format!("SP_EMU_DUMP_ARCHIVE_ID={archive_id}");

    // Arm the service with the dump dir + archive id if it isn't already. The env
    // only takes effect on (re)start, so a first-time arm costs one ~30s preboot;
    // an already-armed SP triggers immediately. Same svccfg-file edit as sp_debug.
    let env = ssh_capture(
        &ip,
        &zlogin(&format!("svcprop -p start/environment {fmri}")),
    )
    .map(|s| s.trim().to_string())
    .filter(|s| !s.is_empty())
    .ok_or_else(|| {
        clear_cached_ip(&sw);
        anyhow!("couldn't read {fmri} env on {sw} - is this a running --emu rack?")
    })?;
    let armed = env.split_whitespace().any(|t| t == want_dir)
        && env.split_whitespace().any(|t| t == want_id);
    if !armed {
        let mut tokens: Vec<String> = env
            .split_whitespace()
            .filter(|t| {
                !t.starts_with("SP_EMU_DUMP_DIR=") && !t.starts_with("SP_EMU_DUMP_ARCHIVE_ID=")
            })
            .map(String::from)
            .collect();
        tokens.push(want_dir.clone());
        tokens.push(want_id.clone());
        let quoted: Vec<String> = tokens.iter().map(|t| format!("\"{t}\"")).collect();
        let content = format!(
            "select {fmri}\nsetprop start/environment = astring: ({})\n",
            quoted.join(" ")
        );
        let local = std::env::temp_dir().join(format!("voxel-sp-dumpenv-{port}.scfg"));
        std::fs::write(&local, &content).map_err(|e| anyhow!("write {}: {e}", local.display()))?;
        let remote_gz = format!("{SWITCH_ZONE_ROOT}/var/tmp/voxel-sp-dumpenv-{port}.scfg");
        let remote = format!("/var/tmp/voxel-sp-dumpenv-{port}.scfg");
        if !scp_to(&ip, local.to_str().unwrap_or_default(), &remote_gz) {
            clear_cached_ip(&sw);
            return Err(anyhow!("scp of the svccfg file into {sw} failed"));
        }
        let apply = zlogin(&format!(
            "'svccfg -f {remote} && svcadm refresh {fmri} && svcadm restart {fmri} && rm -f {remote} && echo APPLIED_OK'"
        ));
        let out = ssh_output(&ip, &format!("{apply} 2>&1")).unwrap_or_default();
        if !out.contains("APPLIED_OK") {
            return Err(anyhow!("failed to arm dump on {sw}: {}", out.trim()));
        }
        eprintln!(
            "[voxel] armed {target} (port {port}) for dumps on {sw}; sp-emu restarting, waiting for boot (~30s) ..."
        );
        // Block until the SP answers MGS again (sp_faux retries out the preboot).
        let _ = sp_faux(cfg, name, switch, target, &["state"]).await?;
    }

    // Trigger the dump in-zone, wait for `.done`, and zip the artifact (dump.json +
    // 0x*.bin at the zip root - exactly what `humility hydrate` reads).
    eprintln!("[voxel] triggering dump of {target} on {sw} ...");
    let trigger = format!(
        "set -e; D={dump_dir}; mkdir -p $D; rm -f $D/.done $D/dump.zip $D/dump.json $D/0x*.bin; \
         touch $D/.trigger; i=0; while [ ! -f $D/.done ] && [ $i -lt 40 ]; do sleep 0.5; i=$((i+1)); done; \
         if [ ! -f $D/.done ]; then echo DUMP_TIMEOUT; exit 1; fi; \
         cd $D && zip -q dump.zip dump.json 0x*.bin && echo DUMP_OK"
    );
    let out =
        ssh_output(&ip, &zlogin(&format!("{} 2>&1", shell_quote(&trigger)))).unwrap_or_default();
    if !out.contains("DUMP_OK") {
        return Err(anyhow!("dump trigger failed on {sw}: {}", out.trim()));
    }

    // Pull the zip to the host and decode it there (humility + archive live here).
    let host_dir = std::env::temp_dir().join(format!("voxel-spdump-{port}"));
    std::fs::create_dir_all(&host_dir).map_err(|e| anyhow!("mkdir {}: {e}", host_dir.display()))?;
    let zip_local = host_dir.join("dump.zip");
    let zip_remote = format!("{SWITCH_ZONE_ROOT}{dump_dir}/dump.zip");
    if !scp_from(&ip, &zip_remote, zip_local.to_str().unwrap_or_default()) {
        return Err(anyhow!("couldn't pull the dump zip from {sw}"));
    }
    let hydrated = host_dir.join("hydrated.dump");
    // humility hydrate refuses to overwrite its `-o` target, so clear a stale one
    // from a previous dump of this SP.
    let _ = std::fs::remove_file(&hydrated);
    eprintln!("[voxel] hydrating with `{humility} -a {archive}` ...");
    let hy = std::process::Command::new(&humility)
        .args(["-a", archive, "hydrate"])
        .arg(&zip_local)
        .arg("-o")
        .arg(&hydrated)
        .status();
    match hy {
        Ok(s) if s.success() => {}
        Ok(s) => {
            return Err(anyhow!(
                "humility hydrate exited {s} (archive/image-id mismatch?)"
            ));
        }
        Err(e) => {
            return Err(anyhow!(
                "couldn't run humility (`{humility}`): {e} - put humility on PATH or set $VOXEL_HUMILITY"
            ));
        }
    }
    // The hydrated dump is self-contained (humility `-d`); no archive needed to decode.
    let cmd = if ringbuf { "ringbuf" } else { "tasks" };
    let dec = std::process::Command::new(&humility)
        .arg("-d")
        .arg(&hydrated)
        .arg(cmd)
        .output()
        .map_err(|e| anyhow!("couldn't run humility {cmd}: {e}"))?;
    print!("{}", String::from_utf8_lossy(&dec.stdout));
    eprint!("{}", String::from_utf8_lossy(&dec.stderr));
    eprintln!(
        "[voxel] dump saved: {} - inspect further with `{humility} -d {} <ringbuf|readvar|...>`",
        hydrated.display(),
        hydrated.display()
    );
    Ok(())
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
    let combined = ssh_capture(&ip, &zlogin(&shell_quote(&probe))).unwrap_or_default();
    let outputs: Vec<String> = {
        let mut v = vec![String::new(); ports.len()];
        let mut idx: Option<usize> = None;
        for line in combined.lines() {
            if let Some(rest) = line.strip_prefix("@@SP ") {
                idx = rest
                    .trim()
                    .parse::<u16>()
                    .ok()
                    .and_then(|p| ports.iter().position(|&q| q == p));
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
        println!(
            "{:<8}  {:<5}  {:<8}  {:<12}  {:<6}  {}",
            sp.selector(),
            sp.base_port,
            typ,
            serial,
            power,
            archive
        );
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
            println!(
                "  {name:<14} {p}  [{}]",
                if present(p) { "present" } else { "MISSING" }
            )
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
        if ready {
            "yes"
        } else {
            "no - set/fix the paths above (or `voxel sp build`)"
        }
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
    eprintln!(
        "[voxel] building sp-emu hubris images for {commit} via {}",
        script.display()
    );
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
    crate::util::locate_script("VOXEL_BUILD_SP", "build-sp.sh")
}
