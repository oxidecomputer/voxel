// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

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

use anyhow::{Context, anyhow, bail};
use camino::{Utf8Path, Utf8PathBuf};
use voxel_config::VoxelConfig;
use voxel_config::sp::{SP_PORT_BASE, Sp, SpFleet, SpRole};

use crate::SpCmd;
use crate::access::resolve_switch;
use crate::topo::{Topo, build_topo};

pub(crate) async fn cmd_sp(
    cfg: &VoxelConfig,
    name: &str,
    cmd: &SpCmd,
) -> anyhow::Result<()> {
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
        SpCmd::Exec { target, switch, command } => {
            // `command` is the passthrough token(s) after `-e`. Split each on
            // whitespace so a single quoted string (`-e "read-caboose 0"`) and
            // separate args (`-e read-caboose 0`) both flatten to faux-mgs argv.
            let parts: Vec<&str> =
                command.iter().flat_map(|s| s.split_whitespace()).collect();
            print!("{}", sp_faux(cfg, name, switch, target, &parts).await?);
            Ok(())
        }
        SpCmd::Reflash { target, image, switch } => {
            sp_reflash(cfg, name, switch, target, image).await
        }
        SpCmd::Debug { target, off, switch } => {
            sp_debug(cfg, name, switch, target, *off).await
        }
        SpCmd::Dump { target, ringbuf, switch } => {
            sp_dump(cfg, name, switch, target, *ringbuf).await
        }
        SpCmd::Ipcc { target, cmd, switch } => {
            sp_ipcc(cfg, name, switch, target, cmd).await
        }
    }
}

// --- operator commands (faux-mgs in the switch zone) -----------------------

/// The MGS loopback port for an SP target. Accepts (in order): a node selector
/// (`sidecar` | `g0` | `g1` ...), a board serial (e.g. `2FAKE001`), or a raw
/// sim address (`[::1]:33310` | `33310`).
fn resolve_port(fleet: &SpFleet, target: &str) -> anyhow::Result<u16> {
    if let Some(sp) = fleet.sps.iter().find(|sp| sp.selector() == target) {
        return Ok(sp.base_port);
    }
    if let Some(sp) = fleet.sps.iter().find(|sp| sp_serial(sp) == target) {
        return Ok(sp.base_port);
    }
    if let Some(p) =
        target.rsplit(':').next().and_then(|s| s.parse::<u16>().ok())
    {
        return Ok(p);
    }
    Err(anyhow!(
        "unknown SP target {target:?}: expected a serial (e.g. 2FAKE001), a node \
         (sidecar | g0 | g1 ...), or a sim addr ([::1]:33310 | 33310)"
    ))
}

/// The board serial an SP reports: the fleet's configured serial, which voxel
/// feeds to the SP itself (sp-sim config / sp-emu SP_EMU_VPD_SERIAL).
fn sp_serial(sp: &Sp) -> String {
    sp.serial.clone()
}

/// Run a faux-mgs command against one SP (by port) inside the switch zone over
/// ssh, returning its combined output. The emulator can drop the first request
/// under load, so retry (callers pick how hard). Uses [`ssh_output`] (not
/// `ssh_capture`) so a non-zero faux-mgs exit returns the SP's OWN error text
/// (e.g. "the image caboose does not contain 'GITC'", "code: Unconfigured") - a
/// bad arg or an empty slot is the SP answering, not the rack being down. Only a
/// genuine ssh transport failure (None) maps to "is the switch zone reachable".
/// The rack, switch slot and SP fleet behind a `--switch` selector. The slot is
/// the scrimlet's position among its rack's scrimlets, which is what mgs.rs
/// numbers switch0/switch1 by and which view port the host fleet answers on.
fn switch_target(
    cfg: &VoxelConfig,
    topo: &Topo,
    switch: &str,
) -> anyhow::Result<(SpFleet, usize, u16, String)> {
    let (s, _) = resolve_switch(topo, switch)?;
    let slot = topo
        .sleds
        .iter()
        .filter(|(d, _)| d.scrimlet && d.rack == s.rack)
        .position(|(d, _)| d.name == s.name)
        .unwrap_or(0) as u16;
    // Built the same way launch and the host fleet build it, so the operator
    // commands cannot disagree with the SPs that are actually running.
    Ok((crate::topo::emu_fleet(cfg, s.rack), s.rack, slot, s.name.clone()))
}

/// The faux-mgs to drive the fleet with: the copy staged beside the rack's
/// fleet, else `[sp].faux_mgs`, else the pinned buildomat build.
fn faux_bin(cfg: &VoxelConfig, rack: usize) -> anyhow::Result<Utf8PathBuf> {
    let staged = crate::topo::sp_fleet_dir(rack).join("sp-emu/faux-mgs");
    if staged.exists() {
        return Ok(staged);
    }
    crate::sp_host::ensure_faux_mgs(cfg)
}

/// Run a faux-mgs verb against the rack's host fleet. The fleet runs here, so
/// this is a plain local process: no switch zone, no ssh hop, no cached
/// scrimlet IP to go stale. faux-mgs logs to stderr and answers on stdout, so
/// combine them the way the in-zone `2>&1` did.
fn faux_run(
    bin: &Utf8Path,
    addr: &str,
    port: u16,
    args: &[&str],
    attempts: u32,
    timeout_ms: u32,
) -> anyhow::Result<String> {
    let out = std::process::Command::new(bin.as_str())
        .arg("--sp-sim-addr")
        .arg(format!("[{addr}]:{port}"))
        .arg("--max-attempts")
        .arg(attempts.to_string())
        .arg("--per-attempt-timeout-millis")
        .arg(timeout_ms.to_string())
        .args(args)
        .output()
        .with_context(|| format!("run {bin}"))?;
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    if text.trim().is_empty() {
        bail!(
            "faux-mgs said nothing for SP port {port} at [{addr}] - is the \
             fleet up? (svcs -a | grep voxel-sp-emu)"
        );
    }
    Ok(text)
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
    let (fleet, rack, slot, _) = switch_target(cfg, &topo, switch)?;
    // Each SP serves switch0 on its base port and switch1 on the next one.
    let port = resolve_port(&fleet, target)? + slot;
    let bin = faux_bin(cfg, rack)?;
    // 30s per attempt: sprot-backed verbs (`state`, rot-*) take the emu gimlets
    // ~20s under post-init MGS load; 15s timed out on every attempt.
    faux_run(
        &bin,
        &voxel_config::config::sp_host_addr(rack),
        port,
        args,
        3,
        30000,
    )
}

/// `voxel sp reflash <target> <image>` - re-flash a live SP (or the shared RoT)
/// and restart its sp-emu service: the firmware counterpart to `voxel rack
/// patch`. Flashes in-zone with the BAKED `sp-emu` (so it works on a
/// self-contained image with no `[sp]` paths set), then verifies over MGS.
/// Live + ephemeral - a clean relaunch reverts to the image; bake it via
/// `build-cp.sh` to persist. `target == "rot"` swaps the shared raw
/// `rot.flash` (every RoT bridge serves it) and restarts them all; otherwise
/// it's a single SP.
async fn sp_reflash(
    cfg: &VoxelConfig,
    name: &str,
    switch: &str,
    target: &str,
    image: &Utf8Path,
) -> anyhow::Result<()> {
    if !image.exists() {
        return Err(anyhow!("image not found: {}", image));
    }
    let topo = build_topo(cfg, name)?;
    let (fleet, rack, _, _) = switch_target(cfg, &topo, switch)?;
    // Fails loudly when this is not a running --emu rack: there is no flash
    // file or voxel-sp-emu instance to swap on an sp-sim one.
    crate::sp_host::emu_bin(rack)?;

    if target == "rot" {
        // Every SP runs oxide-rot-1 in-process from the fleet's shared RoT
        // image, so replacing it and restarting the fleet reflashes them all.
        let rot = crate::sp_host::fleet_dir(rack).join("rot.image");
        eprintln!("[voxel] reflashing the shared RoT ({rot}) from {image}");
        std::fs::copy(image, &rot)
            .with_context(|| format!("copy {image} to {rot}"))?;
        let n = crate::sp_host::restart_rack(rack);
        if n == 0 {
            return Err(anyhow!(
                "RoT image placed at {rot} but no sp-emu instance restarted"
            ));
        }
        eprintln!(
            "[voxel] restarted {n} SP(s); verifying via rot-boot-info ..."
        );
        match sp_faux(cfg, name, switch, "sidecar", &["rot-boot-info"]).await {
            Ok(o) => print!("{o}"),
            Err(e) => eprintln!(
                "[voxel] RoT reflashed; rot-boot-info not yet available ({e})"
            ),
        }
        return Ok(());
    }

    // The SMF instance is the SP process, one per base port; the switch slot
    // only picks which view to talk to.
    let port = resolve_port(&fleet, target)?;
    eprintln!("[voxel] reflashing SP {target} (port {port}) from {image}");
    crate::sp_host::flash_sp(rack, port, image)?;
    // The SP re-runs its ~340M-instruction preboot (~30s) before MGS answers;
    // sp_faux retries generously, so this waits out the boot and returns fresh
    // state.
    eprintln!(
        "[voxel] SP {target} reflashed + restarting; waiting for it to boot \
         (~30s) ..."
    );
    match sp_faux(cfg, name, switch, target, &["state"]).await {
        Ok(o) => print!("{o}"),
        Err(e) => eprintln!(
            "[voxel] reflashed, but SP {target} not responding yet ({e}); \
             retry `voxel sp info {target}` shortly"
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
    let (fleet, rack, _, _) = switch_target(cfg, &topo, switch)?;
    // The SMF instance is the SP process, one per base port; the switch slot
    // only picks which view to TALK to, which is not what we are toggling.
    let port = resolve_port(&fleet, target)?;
    // Keep everything else in the environment (board, state dir, bridge, RoT)
    // and toggle only SP_EMU_NO_DEBUG.
    let env = crate::sp_host::read_env(rack, port).ok_or_else(|| {
        anyhow!(
            "couldn't read the SMF environment for SP port {port} - is this a \
             running --emu rack?"
        )
    })?;
    let mut tokens: Vec<String> = env
        .split_whitespace()
        .filter(|t| !t.starts_with("SP_EMU_NO_DEBUG"))
        .map(String::from)
        .collect();
    if off {
        tokens.push("SP_EMU_NO_DEBUG=1".to_string());
    }
    crate::sp_host::set_env(rack, port, &tokens)?;
    if off {
        eprintln!(
            "[voxel] {target} (port {port}) debug DISABLED; sp-emu restarting \
             (listeners off, production mode)"
        );
        return Ok(());
    }
    // Per-SP ports, offset by the bridge port so every SP in the fleet is
    // debuggable at once (sp-emu gdb.rs: 33300 -> 0, 33310 -> 10, ...). sp-emu
    // exposes a Glasgow SWD probe, which stock humility speaks directly: the SP
    // at 4444 + off and its RoT at 4544 + off.
    let off = port.wrapping_sub(SP_PORT_BASE);
    let (sp_swd, rot_swd) = (4444 + off, 4544 + off);
    eprintln!(
        "[voxel] {target} (port {port}) debug ENABLED; sp-emu restarting \
         (listeners ready in ~30s after preboot)"
    );
    // The fleet runs here, so the listeners are on this host's loopback and
    // humility runs here too - no zone to tunnel into.
    println!("humility attach (listeners on 127.0.0.1 of this host):");
    println!(
        "  SP   - humility -a <archive.zip> -p 20b7:9db1:tcp:127.0.0.1:{sp_swd} <cmd>"
    );
    println!(
        "  RoT  - humility -a <rot-archive.zip> -p 20b7:9db1:tcp:127.0.0.1:{rot_swd} <cmd>"
    );
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
    if !matches!(command, "identity" | "bsu" | "macs" | "status" | "inventory")
    {
        return Err(anyhow!(
            "--cmd must be one of identity|bsu|macs|status|inventory (got \
             `{command}`)"
        ));
    }
    let topo = build_topo(cfg, name)?;
    let (fleet, rack, _, _) = switch_target(cfg, &topo, switch)?;
    let port = resolve_port(&fleet, target)?;
    // sp-emu carries the ipcc subcommands, and it is the same binary already
    // running the fleet.
    let bin = crate::sp_host::emu_bin(rack)?;
    let state = crate::sp_host::state_dir(rack, port);
    let sp_sock = state.join("ipcc.sock");
    let ctl_sock = state.join("ipcc-ctl.sock");

    eprintln!("[voxel] {target} (port {port}): ipcc {command}");
    // Fast path: a broker is already up and holding the SP's UART, so repeats
    // skip the reboot entirely.
    let started_broker = match ipcc_req(&bin, &ctl_sock, command) {
        IpccReply::Reply(out) => {
            print!("{out}");
            return Ok(());
        }
        IpccReply::Unsupported => {
            return Err(anyhow!(
                "{bin} has no ipcc broker (no ipcc-serve / ipcc-req \
                 subcommands), so there is nothing to drive the SP's host \
                 UART with - point [sp].emu_bin at an sp-emu built from the \
                 host-UART line"
            ));
        }
        IpccReply::NoBroker => true,
        IpccReply::NotConnected => false,
    };
    if started_broker {
        let log = std::fs::File::create(state.join("ipcc-broker.log"))
            .with_context(|| format!("open the IPCC broker log in {state}"))?;
        // Detached, so it outlives this command and later calls take the fast
        // path above.
        std::process::Command::new(bin.as_str())
            .args(["ipcc-serve", sp_sock.as_str(), ctl_sock.as_str()])
            .stdin(std::process::Stdio::null())
            .stderr(log.try_clone().context("clone the broker log handle")?)
            .stdout(log)
            .spawn()
            .with_context(|| {
                format!("start the IPCC broker for port {port}")
            })?;
        std::thread::sleep(std::time::Duration::from_secs(1));
    }

    // Point the SP's UART7 at the broker socket. The environment only takes
    // effect on (re)start, and a brand new broker needs the SP to reconnect
    // either way.
    let env = crate::sp_host::read_env(rack, port).ok_or_else(|| {
        anyhow!(
            "couldn't read the SMF environment for SP port {port} - is this a \
             running --emu rack?"
        )
    })?;
    let want = format!("SP_EMU_HOST_UART={sp_sock}");
    if env.split_whitespace().any(|t| t == want) {
        if started_broker {
            crate::sp_host::restart(rack, port);
        }
    } else {
        let mut tokens: Vec<String> = env
            .split_whitespace()
            .filter(|t| !t.starts_with("SP_EMU_HOST_UART="))
            .map(String::from)
            .collect();
        tokens.push(want);
        crate::sp_host::set_env(rack, port, &tokens)?;
    }

    // Wait out the SP's ~30s preboot, then read the reply through the broker.
    for _ in 0..70 {
        if let IpccReply::Reply(out) = ipcc_req(&bin, &ctl_sock, command) {
            print!("{out}");
            if !out.contains("SpToHost reply") {
                return Err(anyhow!(
                    "no decoded IPCC reply (the SP may still be booting - \
                     retry `voxel sp ipcc {target}`)"
                ));
            }
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
    Err(anyhow!("IPCC timed out waiting for {target} to answer {command}"))
}

/// What one IPCC request through the broker produced.
enum IpccReply {
    /// A decoded SpToHost reply.
    Reply(String),
    /// No broker is listening (sp-emu exit 3).
    NoBroker,
    /// The broker is up but the SP has not connected yet (sp-emu exit 4).
    NotConnected,
    /// This sp-emu has no ipcc broker at all. It prints its usage and exits 0,
    /// which would otherwise read as an empty success.
    Unsupported,
}

/// One IPCC request through a running broker.
fn ipcc_req(bin: &Utf8Path, ctl: &Utf8Path, command: &str) -> IpccReply {
    let Ok(out) = std::process::Command::new(bin.as_str())
        .args(["ipcc-req", ctl.as_str(), command])
        .output()
    else {
        return IpccReply::NoBroker;
    };
    // sp-emu answers on stdout but prints its usage (and its identity
    // banner) on stderr, so judge the run on both.
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    if !out.status.success() {
        return match out.status.code() {
            Some(4) => IpccReply::NotConnected,
            _ => IpccReply::NoBroker,
        };
    }
    if text.contains("sp-emu flash") && text.contains("usage:") {
        return IpccReply::Unsupported;
    }
    IpccReply::Reply(text)
}

/// `voxel sp dump <target> [--ringbuf]` - force + decode a crash dump of one live
/// emulated SP. sp-emu writes a humility-hydrate RAM snapshot on demand: when
/// `<SP_EMU_DUMP_DIR>/.trigger` appears it dumps RAM (flash comes from the archive)
/// and swaps in `.done` (src/mem.rs `write_hydrate_dump`). We arm the SP's service
/// with that dir + its archive id (a one-time ~30s restart if not already armed),
/// touch the trigger in-zone, pull the zip to the host, and run `humility
/// hydrate` + `tasks`/`ringbuf` against the SP's hubris archive - all where
/// humility + the archive live (the debug listeners are in-zone loopback, so a
/// probe-based dump would need a tunnel; this needs none). Live + ephemeral:
/// the arming reverts on a clean relaunch. Emu-only (sp-sim never faults / has
/// no dump dir).
async fn sp_dump(
    cfg: &VoxelConfig,
    name: &str,
    switch: &str,
    target: &str,
    ringbuf: bool,
) -> anyhow::Result<()> {
    let topo = build_topo(cfg, name)?;
    let (fleet, rack, _, _) = switch_target(cfg, &topo, switch)?;
    // The SMF instance is the SP process, one per base port.
    let port = resolve_port(&fleet, target)?;
    let selector = fleet
        .sps
        .iter()
        .find(|s| s.base_port == port)
        .map(|s| s.selector())
        .unwrap_or_else(|| target.to_string());
    // Fails loudly when this is not a running --emu rack: sp-sim has no dump
    // directory to arm.
    crate::sp_host::emu_bin(rack)?;

    // The hubris archive for this SP: humility needs it to fill flash (the dump
    // omits it) and its image id must match the dump's. Take the archive the
    // fleet was flashed from, so it cannot drift from what is running.
    let board = if selector == "sidecar" { "sidecar" } else { "gimlet" };
    let archive =
        crate::sp_host::fleet_dir(rack).join(format!("{board}.archive"));
    if !archive.exists() {
        return Err(anyhow!(
            "no hubris archive at {archive} - is this a running --emu rack?"
        ));
    }
    let humility = std::env::var("VOXEL_HUMILITY")
        .unwrap_or_else(|_| "humility".to_string());

    // The dump's archive id must equal the archive's image id (humility hydrate
    // rejects a mismatch), so read the SP's running archive id over MGS.
    let state = sp_faux(cfg, name, switch, target, &["state"]).await?;
    let archive_id = state
        .lines()
        .find_map(|l| l.split("hubris archive:").nth(1))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            anyhow!(
                "couldn't read {target}'s hubris archive id from `sp state`"
            )
        })?;

    let dump_dir = crate::sp_host::state_dir(rack, port).join("dump");
    let want_dir = format!("SP_EMU_DUMP_DIR={dump_dir}");
    let want_id = format!("SP_EMU_DUMP_ARCHIVE_ID={archive_id}");

    // Arm the instance with the dump dir + archive id if it isn't already. The
    // environment only takes effect on (re)start, so a first-time arm costs one
    // ~30s preboot; an already-armed SP triggers immediately.
    let env = crate::sp_host::read_env(rack, port).ok_or_else(|| {
        anyhow!(
            "couldn't read the SMF environment for SP port {port} - is this a \
             running --emu rack?"
        )
    })?;
    let armed = env.split_whitespace().any(|t| t == want_dir)
        && env.split_whitespace().any(|t| t == want_id);
    if !armed {
        let mut tokens: Vec<String> = env
            .split_whitespace()
            .filter(|t| {
                !t.starts_with("SP_EMU_DUMP_DIR=")
                    && !t.starts_with("SP_EMU_DUMP_ARCHIVE_ID=")
            })
            .map(String::from)
            .collect();
        tokens.push(want_dir);
        tokens.push(want_id);
        crate::sp_host::set_env(rack, port, &tokens)?;
        eprintln!(
            "[voxel] armed {target} (port {port}) for dumps; sp-emu \
             restarting, waiting for boot (~30s) ..."
        );
        // Block until the SP answers MGS again (sp_faux retries out the preboot).
        let _ = sp_faux(cfg, name, switch, target, &["state"]).await?;
    }

    // The fleet runs here, so the dump lands in the SP's own state directory.
    eprintln!("[voxel] triggering dump of {target} ...");
    std::fs::create_dir_all(&dump_dir)
        .with_context(|| format!("mkdir {dump_dir}"))?;
    let done = dump_dir.join(".done");
    let zip_local = dump_dir.join("dump.zip");
    for stale in [&done, &zip_local, &dump_dir.join("dump.json")] {
        let _ = std::fs::remove_file(stale);
    }
    if let Ok(entries) = std::fs::read_dir(&dump_dir) {
        for e in entries.flatten() {
            let n = e.file_name().to_string_lossy().into_owned();
            if n.starts_with("0x") && n.ends_with(".bin") {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
    std::fs::write(dump_dir.join(".trigger"), "")
        .with_context(|| format!("trigger a dump in {dump_dir}"))?;
    let mut waited_ms = 0;
    while !done.exists() {
        if waited_ms >= 20_000 {
            return Err(anyhow!(
                "timed out waiting for {target} to write its dump in {dump_dir}"
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
        waited_ms += 500;
    }
    // humility hydrate reads dump.json + 0x*.bin from the zip root. Local sh, so
    // the glob is the shell's own with no nested quoting to survive.
    let zipped = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("cd {dump_dir} && zip -q dump.zip dump.json 0x*.bin"))
        .status()
        .with_context(|| format!("zip the dump in {dump_dir}"))?;
    if !zipped.success() {
        return Err(anyhow!(
            "zipping the dump in {dump_dir} failed ({zipped})"
        ));
    }

    let hydrated = dump_dir.join("hydrated.dump");
    // humility hydrate refuses to overwrite its `-o` target, so clear a stale
    // one from a previous dump of this SP.
    let _ = std::fs::remove_file(&hydrated);
    eprintln!("[voxel] hydrating with `{humility} -a {archive}` ...");
    let hy = std::process::Command::new(&humility)
        .args(["-a", archive.as_str(), "hydrate"])
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
                "couldn't run humility (`{humility}`): {e} - put humility on \
                 PATH or set $VOXEL_HUMILITY"
            ));
        }
    }
    // The hydrated dump is self-contained (humility `-d`); no archive needed.
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
        "[voxel] dump saved: {} - inspect further with `{humility} -d {} \
         <ringbuf|readvar|...>`",
        hydrated, hydrated
    );
    Ok(())
}

/// `voxel sp ls` - enumerate every SP via the switch zone, pilot-style table.
async fn sp_ls(
    cfg: &VoxelConfig,
    name: &str,
    switch: &str,
) -> anyhow::Result<()> {
    let topo = build_topo(cfg, name)?;
    let (fleet, rack, slot, sw) = switch_target(cfg, &topo, switch)?;
    let bin = faux_bin(cfg, rack)?;
    let addr = voxel_config::config::sp_host_addr(rack);
    println!("SPs via {sw} (host fleet [{addr}]):");
    println!(
        "{:<8}  {:<5}  {:<8}  {:<12}  {:<6}  ARCHIVE",
        "SP", "PORT", "TYPE", "SERIAL", "POWER"
    );
    // The fleet runs here, so probe every SP concurrently as its own process.
    // A warm SP answers in ~20ms, which makes the table one probe deep rather
    // than the sum of them. `state` includes a sprot round trip to the RoT,
    // which takes the emu gimlets ~20s under post-init MGS load, so each
    // attempt needs headroom beyond that.
    let kids: Vec<_> = fleet
        .sps
        .iter()
        .map(|sp| {
            std::process::Command::new(bin.as_str())
                .arg("--sp-sim-addr")
                .arg(format!("[{addr}]:{}", sp.base_port + slot))
                .arg("--max-attempts")
                .arg("2")
                .arg("--per-attempt-timeout-millis")
                .arg("30000")
                .arg("state")
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .spawn()
        })
        .collect();
    let outputs: Vec<String> = kids
        .into_iter()
        .map(|k| {
            k.ok()
                .and_then(|c| c.wait_with_output().ok())
                .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
                .unwrap_or_default()
        })
        .collect();
    let mut answered = false;
    for (sp, out) in fleet.sps.iter().zip(outputs) {
        let typ = match sp.role {
            SpRole::Sidecar => "sidecar",
            SpRole::Gimlet(_) => "gimlet",
        };
        // An SP that answered has a hubris archive / power even if its serial is
        // blank, so key "answered" off that, not serial.
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
            sp.base_port + slot,
            typ,
            serial,
            power,
            archive
        );
    }
    if !answered {
        eprintln!(
            "[voxel] no SP answered - the rack may be mid-bring-up, the SPs busy \
             under MGS load (retry), or the fleet may not be running (svcs -a | \
             grep voxel-sp-emu)."
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

fn ready(cfg: &VoxelConfig) {
    println!("sp-emu binaries ([sp] overrides, else pinned buildomat builds):");
    let emu = crate::sp_host::ensure_emu_bin(cfg);
    let faux = crate::sp_host::ensure_faux_mgs(cfg);
    for (name, r) in [("emu_bin", &emu), ("faux_mgs", &faux)] {
        match r {
            Ok(p) => println!("  {name:<14} {p}"),
            Err(e) => println!("  {name:<14} unavailable ({e:#})"),
        }
    }
    // Firmware is not listed: an image built with --from-tuf carries the
    // release's own, and --sp-firmware overrides it for one launch.
    println!(
        "\n`voxel launch --emu` ready: {}",
        if emu.is_ok() {
            "yes (firmware comes from the image, or --sp-firmware)"
        } else {
            "no - see above, or set [sp].emu_bin to an sp-emu binary"
        }
    );
}

fn flash(
    cfg: &VoxelConfig,
    image: &Utf8Path,
    out: &Utf8Path,
) -> anyhow::Result<()> {
    let emu_bin = crate::sp_host::ensure_emu_bin(cfg)?;
    if !image.exists() {
        return Err(anyhow!("image not found: {}", image));
    }
    eprintln!("[voxel] flashing {} -> {}", image, out);
    let status = std::process::Command::new(&emu_bin)
        .env("SP_EMU_FLASH", out)
        .args(["flash", "a"])
        .arg(image)
        .status()
        .map_err(|e| anyhow!("run {emu_bin}: {e}"))?;
    if !status.success() {
        return Err(anyhow!("sp-emu flash failed"));
    }
    println!("flashed {}", out);
    Ok(())
}

fn build(commit: &str) -> anyhow::Result<()> {
    let script = build_sp_script()?;
    eprintln!(
        "[voxel] building sp-emu hubris images for {commit} via {}",
        script
    );
    let status = std::process::Command::new("bash")
        .arg(&script)
        .arg(commit)
        .status()
        .map_err(|e| anyhow!("run {}: {e}", script))?;
    if !status.success() {
        return Err(anyhow!("build-sp.sh failed for {commit}"));
    }
    Ok(())
}

/// Locate `voxel-image/build-sp.sh` (mirrors `image::build_cp_script`).
fn build_sp_script() -> anyhow::Result<Utf8PathBuf> {
    crate::util::locate_script("VOXEL_BUILD_SP", "build-sp.sh")
}
