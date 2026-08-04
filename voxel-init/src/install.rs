//! Image-BUILD-time install, run inside the builder guest.
//!
//! Replaces the per-image install shell scripts (`install-cp.sh`,
//! `install-frr.sh`): `build-image.sh` runs `voxel-init install --role <role>`
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

/// Write the marker `build-image.sh` greps to confirm the install ran to
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

/// Helios control-plane image. Not ported yet: `build-cp.sh` still runs
/// `install-cp.sh`. Ported after `frr` proves the shape, so the sp-emu bake it
/// currently carries can be dropped in the same pass rather than translated.
pub fn cp() -> Result<()> {
    bail!("install --role cp is not ported yet; build-cp.sh still runs install-cp.sh")
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
