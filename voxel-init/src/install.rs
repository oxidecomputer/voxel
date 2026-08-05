//! Image-BUILD-time install, run inside the builder guest.
//!
//! Replaces the per-image install shell scripts (`install-cp.sh`,
//! `install-frr.sh`): `voxel image bake` runs `voxel-init install --role <role>`
//! in the builder node, which installs baked software and applies NO
//! topology-specific configuration. Per-topology config is generated in Rust and
//! pushed at LAUNCH by the `gimlet` / `router` roles.
//!
//! Same source builds for both guest OSes; the role picks the implementation at
//! runtime, matching how `gimlet` / `router` already work.

use crate::sys::{capture, note, replace_in_file, run, run_quiet, warn};
use anyhow::{Result, bail};
use std::fs;
use std::path::Path;

const READY_MARKER: &str = "/var/voxel-image-ready";
const CARGO_BAY: &str = "/opt/cargo-bay";
const AGENT_DST: &str = "/opt/oxide/voxel-init";

/// The static address staged as `builder-net` when the host built this image on
/// an isolated external segment (no DHCP server to lease from).
struct BuilderNet {
    cidr: String,
    gateway: String,
}

/// Read `<cidr> <gateway>` from the cargo-bay. `None` when the host used the
/// default LAN path and the builder just DHCPs.
fn builder_net() -> Option<BuilderNet> {
    let text = fs::read_to_string(format!("{CARGO_BAY}/builder-net")).ok()?;
    let mut parts = text.split_whitespace();
    let cidr = parts.next()?.to_string();
    let gateway = parts.next()?.to_string();
    Some(BuilderNet { cidr, gateway })
}

/// Copy the staged agent onto local disk and make it executable. The cargo-bay
/// 9p mount drops the exec bit, so this cannot be a symlink or a direct run.
/// Fatal: an image without the agent boots into nothing at launch.
fn bake_agent() -> Result<()> {
    let src = format!("{CARGO_BAY}/voxel-init");
    if !Path::new(&src).exists() {
        bail!("voxel-init not staged at {src}");
    }
    note("baking voxel-init agent");
    fs::create_dir_all("/opt/oxide").ok();
    fs::copy(&src, AGENT_DST).map_err(|e| anyhow::anyhow!("copy {src} -> {AGENT_DST}: {e}"))?;
    run("chmod", &["+x", AGENT_DST]);
    Ok(())
}

/// Write the marker `voxel image bake` greps to confirm the install ran to
/// completion before it captures the disk.
fn mark_ready(body: &str) -> Result<()> {
    run_quiet("sync", &[]);
    let built = capture("date", &["+%Y-%m-%dT%H:%M:%S"]).unwrap_or_default();
    let line = format!("{body} built={built}\n");
    fs::write(READY_MARKER, &line)
        .map_err(|e| anyhow::anyhow!("write {READY_MARKER}: {e}"))?;
    note(format!("image ready: {}", line.trim()));
    Ok(())
}

/// The switch-slot enforcer, baked as an SMF service. The 2nd scrimlet of each
/// rack must present as switch1, but one image bakes switch0 for everyone and
/// voxel-init swaps the live config at bring-up. Doing that from a one-shot
/// detached process is fragile: if the sled restarts or the process dies under
/// load before the swap, the scrimlet silently reverts to switch0 and its rack's
/// Nexus handoff wedges ("switch-port qsfp0 not found"). As an SMF service,
/// startd re-runs it at EVERY boot and restarts it if it dies.
const SWITCH_ENFORCER_MANIFEST: &str = r#"<?xml version="1.0"?>
<!DOCTYPE service_bundle SYSTEM "/usr/share/lib/xml/dtd/service_bundle.dtd.1">
<service_bundle type='manifest' name='voxel-switch-enforcer'>
  <service name='oxide/voxel-switch-enforcer' type='service' version='1'>
    <create_default_instance enabled='true'/>
    <single_instance/>
    <dependency name='fs-local' grouping='require_all' restart_on='none' type='service'>
      <service_fmri value='svc:/system/filesystem/local:default'/>
    </dependency>
    <exec_method type='method' name='start'
      exec='/opt/oxide/voxel-init switch-enforcer-svc'
      timeout_seconds='1800'/>
    <exec_method type='method' name='stop' exec=':true' timeout_seconds='60'/>
    <property_group name='startd' type='framework'>
      <propval name='duration' type='astring' value='transient'/>
    </property_group>
    <stability value='Unstable'/>
    <template>
      <common_name><loctext xml:lang='C'>voxel switch-slot enforcer</loctext></common_name>
    </template>
  </service>
</service_bundle>
"#;

fn sleep2() {
    std::thread::sleep(std::time::Duration::from_secs(2));
}

/// Helios control-plane image: install pinned deps, unpack the control-plane
/// zone artifacts, bake what launch needs. Applies NO topology configuration -
/// config-rss injection, sprockets keys, SMBIOS identity and RSS all happen at
/// launch.
///
/// Deliberately NOT baked, kept ephemeral or per-launch: `xtask
/// virtual-hardware create` (per-node emulated U.2/M.2), `scadm propolis
/// load-program`, the rpool/dump zvol, and the emulated SP/RoT fleet - flashing
/// hubris images is a runtime concern, so `voxel launch --emu-sp` stages and
/// flashes it per-scrimlet from the `[sp]` config instead.
pub fn cp() -> Result<()> {
    let version =
        std::env::var("VOXEL_CP_VERSION").unwrap_or_else(|_| "unknown".into());

    // --- networking: reach pkg.oxide.computer ---
    // The builder has a single external NIC; find a vioif and address it.
    let ext_if = std::env::var("EXT_IF")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            capture("dladm", &["show-phys", "-o", "link", "-p"]).and_then(|o| {
                o.lines()
                    .find(|l| l.starts_with("vioif"))
                    .map(str::to_string)
            })
        })
        .unwrap_or_else(|| "vioif0".into());
    note(format!("using external interface {ext_if}"));
    fs::write("/etc/resolv.conf", "nameserver 1.1.1.1\n")
        .map_err(|e| anyhow::anyhow!("write /etc/resolv.conf: {e}"))?;

    let addrobj = format!("{ext_if}/v4");
    match builder_net() {
        // Isolated mode: no DHCP server on the segment, use the staged address.
        Some(net) => {
            note(format!(
                "static builder net: {} via {}",
                net.cidr, net.gateway
            ));
            run(
                "ipadm",
                &["create-addr", "-T", "static", "-a", &net.cidr, &addrobj],
            );
            run("route", &["add", "default", &net.gateway]);
        }
        None => {
            run("ipadm", &["create-addr", "-T", "dhcp", &addrobj]);
            note("waiting for DHCP lease...");
            for _ in 0..30 {
                let leased =
                    capture("ipadm", &["show-addr", &addrobj, "-p", "-o", "addr"])
                        .is_some_and(|a| a.contains('/'));
                if leased {
                    break;
                }
                sleep2();
            }
        }
    }
    note("waiting for DNS...");
    for _ in 0..15 {
        if run_quiet("getent", &["hosts", "pkg.oxide.computer"]) {
            break;
        }
        sleep2();
    }

    // --- pinned package deps (baked) ---
    let mut attempt = 0;
    while !run("pkg", &["install", "tofino", "looker", "htop", "jq"]) {
        attempt += 1;
        if attempt >= 25 {
            bail!("pkg install failed after {attempt} attempts");
        }
        note(format!("pkg install attempt {attempt} failed; retrying"));
        sleep2();
    }

    // The builder runs us from /opt/cargo-bay; the staged omicron dir is here.
    let omicron = format!("{CARGO_BAY}/omicron");
    std::env::set_current_dir(&omicron)
        .map_err(|e| anyhow::anyhow!("cd {omicron}: {e}"))?;
    for f in ["omicron-package", "xtask", "xtask-downloader"] {
        run_quiet("chmod", &["+x", f]);
    }
    run_quiet("sh", &["-c", "chmod +x tools/*.sh tools/ci* 2>/dev/null"]);

    // --- control-plane prerequisites + unpack (THE bake) ---
    let xtask = format!("{omicron}/xtask");
    let xtask_dl = format!("{omicron}/xtask-downloader");
    let envs = [
        ("XTASK_BIN", xtask.as_str()),
        ("XTASK_DOWNLOADER_BIN", xtask_dl.as_str()),
    ];
    let mut attempt = 0;
    while !crate::sys::run_env(
        "./tools/install_runner_prerequisites.sh",
        &["-y"],
        &envs,
    ) {
        attempt += 1;
        if attempt >= 5 {
            bail!("install_runner_prerequisites failed after {attempt} attempts");
        }
        note(format!(
            "prerequisites attempt {attempt} failed; retrying in 20s"
        ));
        std::thread::sleep(std::time::Duration::from_secs(20));
    }

    note("unpacking control-plane zone artifacts into /opt/oxide ...");
    if !crate::sys::run_env("./omicron-package", &["--force", "unpack"], &envs) {
        bail!("omicron-package unpack failed");
    }
    let artifacts = capture("find", &["/opt/oxide", "-name", "*.tar.gz"])
        .map(|o| o.lines().filter(|l| !l.trim().is_empty()).count())
        .unwrap_or(0);
    if artifacts == 0 {
        bail!("omicron-package unpack produced no artifacts in /opt/oxide");
    }
    note(format!("unpacked {artifacts} zone artifacts into /opt/oxide"));

    // Strip the default config-rss.toml that omicron v20+ ships in the sled-agent
    // non-gimlet package. sled-agent's SMF auto-starts at boot and would RSS-init
    // from that default (rack_subnet fd00:1122:3344) BEFORE voxel injects its
    // per-launch config - then RSS retries with voxel's and sled-agent refuses
    // ("Sled Agent already running" with a different request).
    fs::remove_file("/opt/oxide/sled-agent/pkg/config-rss.toml").ok();
    note("removed baked default config-rss (RSS will use voxel's injected one)");

    // --- bake launch-time bits ---
    // Bake the WHOLE staged omicron dir: `omicron-package activate` reads
    // out/target/active and `xtask virtual-hardware` needs out/npuzone/, so we
    // can't cherry-pick. The out/*.tar.gz zones duplicate the unpacked
    // /opt/oxide, which is the price of a self-contained activate.
    const BAKE: &str = "/opt/oxide/omicron";
    note(format!("baking omicron CLI dir into {BAKE}"));
    fs::create_dir_all(BAKE).ok();
    if !run("cp", &["-r", ".", BAKE]) {
        bail!("baking omicron dir into {BAKE} failed");
    }
    for f in ["omicron-package", "xtask", "xtask-downloader"] {
        run_quiet("chmod", &["+x", &format!("{BAKE}/{f}")]);
    }
    run_quiet(
        "sh",
        &["-c", &format!("chmod +x {BAKE}/tools/*.sh 2>/dev/null")],
    );

    // SoftNPU sidecar_lite; scrimlets load it into propolis at launch. Staged
    // into the cargo-bay by `voxel image create` because the builder VM may not reach
    // buildomat.eng - only the host does.
    let sc_dir = format!("{CARGO_BAY}/sidecar");
    if Path::new(&format!("{sc_dir}/scadm")).exists()
        && Path::new(&format!("{sc_dir}/libsidecar_lite.so")).exists()
    {
        note("baking sidecar_lite from cargo-bay");
        fs::create_dir_all("/opt/oxide/sidecar").ok();
        for f in ["scadm", "libsidecar_lite.so"] {
            fs::copy(format!("{sc_dir}/{f}"), format!("/opt/oxide/sidecar/{f}"))
                .map_err(|e| anyhow::anyhow!("bake sidecar {f}: {e}"))?;
        }
        run("chmod", &["+x", "/opt/oxide/sidecar/scadm"]);
    } else {
        bail!("sidecar not staged at {sc_dir}");
    }

    bake_agent()?;

    note("baking voxel-switch-enforcer SMF service");
    fs::create_dir_all("/lib/svc/manifest/site").ok();
    let manifest = "/lib/svc/manifest/site/voxel-switch-enforcer.xml";
    fs::write(manifest, SWITCH_ENFORCER_MANIFEST)
        .map_err(|e| anyhow::anyhow!("write {manifest}: {e}"))?;
    if run("svccfg", &["import", manifest]) {
        note("imported voxel-switch-enforcer");
    } else {
        warn("svccfg import voxel-switch-enforcer failed (manifest staged for boot-time import)");
    }

    // Clearing /etc/path_to_inst is the bake's LAST exec before capture;
    // doing it here doesn't stick (later steps regenerate it for this VM).
    mark_ready(&format!(
        "voxel-cp version={version} unpacked_artifacts={artifacts}"
    ))
}

/// Debian router image: install + enable FRR, bake the agent, scrub the
/// builder's DHCP identity. No topology config - `frr.conf` is generated per
/// topology and pushed at launch.
pub fn frr() -> Result<()> {
    let version =
        std::env::var("VOXEL_FRR_VERSION").unwrap_or_else(|_| "unknown".into());

    // --- reach apt ---
    // falcon's default ext link normally gives the node a DHCP NIC. Isolated
    // mode runs no DHCP server on the segment, so apply the staged static
    // address to the first non-loopback NIC instead.
    if let Some(net) = builder_net() {
        let links = capture("ip", &["-o", "link"]).unwrap_or_default();
        let iface = links
            .lines()
            .filter_map(|l| l.split(": ").nth(1))
            .find(|n| *n != "lo")
            .map(str::to_string);
        match iface {
            Some(ifc) => {
                note(format!(
                    "static builder net: {} via {} on {ifc}",
                    net.cidr, net.gateway
                ));
                run("ip", &["link", "set", &ifc, "up"]);
                run("ip", &["addr", "add", &net.cidr, "dev", &ifc]);
                run("ip", &["route", "add", "default", "via", &net.gateway]);
            }
            None => warn("builder-net staged but no ethernet NIC found"),
        }
    }
    // /etc/resolv.conf is a symlink to systemd-resolved's placeholder in
    // isolated mode (no DHCP populated systemd-networkd), so the target file
    // stays empty. Replace the symlink so the nameserver line sticks for apt.
    fs::remove_file("/etc/resolv.conf").ok();
    fs::write("/etc/resolv.conf", "nameserver 1.1.1.1\n")
        .map_err(|e| anyhow::anyhow!("write /etc/resolv.conf: {e}"))?;
    note("waiting for DNS...");
    for _ in 0..15 {
        if run_quiet("getent", &["hosts", "deb.debian.org"]) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_secs(2));
    }

    // apt-daily timers race the apt lock and can wipe FRR state.
    run_quiet(
        "systemctl",
        &["disable", "--now", "apt-daily-upgrade.timer", "apt-daily.timer"],
    );

    // --- install FRR (baked) ---
    let mut attempt = 0;
    loop {
        let ok = run_env_noninteractive(&["update", "-y"])
            && run_env_noninteractive(&[
                "install",
                "-y",
                "frr",
                "frr-pythontools",
                "jq",
                "openssh-server",
            ]);
        if ok {
            break;
        }
        attempt += 1;
        if attempt >= 25 {
            bail!("apt install failed after {attempt} attempts");
        }
        note(format!("apt attempt {attempt} failed; retrying"));
        std::thread::sleep(std::time::Duration::from_secs(2));
    }

    // bgpd + bfdd on (static mode uses BFD-tracked routes); frr.conf itself is
    // generated per topology at launch.
    replace_in_file(
        "/etc/frr/daemons",
        &[("bgpd=no", "bgpd=yes"), ("bfdd=no", "bfdd=yes")],
    );

    // Persistent forwarding; per-interface knobs are set at launch.
    fs::write(
        "/etc/sysctl.d/99-voxel-frr.conf",
        "net.ipv4.ip_forward=1\nnet.ipv6.conf.all.forwarding=1\n",
    )
    .map_err(|e| anyhow::anyhow!("write sysctl conf: {e}"))?;
    run("sysctl", &["-p", "/etc/sysctl.d/99-voxel-frr.conf"]);

    run_quiet("systemctl", &["enable", "frr"]);
    run_quiet("systemctl", &["enable", "ssh"]);

    bake_agent()?;

    // --- scrub the builder's DHCP identity ---
    // The builder may have leased its address over DHCP during this build (LAN
    // mode). A lease DB carried into the image makes every router clone
    // re-request the builder's old address at boot. Wipe the leases and reset
    // machine-id (an empty file regenerates on first boot) so each clone builds
    // a fresh identity. Harmless in isolated mode, which never DHCPs.
    for path in glob_dhclient_leases() {
        fs::remove_file(&path).ok();
    }
    fs::write("/etc/machine-id", "").ok();
    fs::remove_file("/var/lib/dbus/machine-id").ok();

    let frr_ver = capture("dpkg-query", &["-W", "-f=${Version}", "frr"])
        .unwrap_or_else(|| "?".into());
    mark_ready(&format!("voxel-frr version={version} frr={frr_ver}"))
}

/// `apt-get` with the noninteractive frontend the baked install needs.
fn run_env_noninteractive(args: &[&str]) -> bool {
    crate::sys::run_env(
        "apt-get",
        args,
        &[("DEBIAN_FRONTEND", "noninteractive")],
    )
}

/// `/var/lib/dhcp/dhclient*.leases` - the shell used a glob; enumerate instead.
fn glob_dhclient_leases() -> Vec<String> {
    let dir = "/var/lib/dhcp";
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n.starts_with("dhclient") && n.ends_with(".leases"))
        .map(|n| format!("{dir}/{n}"))
        .collect()
}
