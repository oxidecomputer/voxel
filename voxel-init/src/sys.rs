// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Tiny process/file helpers shared by the role agents. The shell scripts these
//! replace ran with `set -x` and (for the gimlet) deliberately not `set -e`:
//! every step is visible and best-effort steps log a warning instead of
//! aborting. Mirror that—`run`/`run_quiet` never panic and return success.

use anyhow::Result;
use camino::Utf8Path;
use std::fs;
use std::process::{Command, Stdio};

/// Parsed `/opt/cargo-bay/external-net` (voxel-managed isolated segment). All
/// fields except `iface` are staged for both sled and router roles.
///
/// Note: `iface` is staged only for routers (sleds self-classify via the jumbo
/// probe).
#[derive(Debug, Default, Clone)]
pub struct ExternalNet {
    /// `<addr>/<prefixlen>` (e.g. `172.30.199.10/24`).
    pub ip_cidr: String,
    /// Default gateway (the host VNIC's address on the etherstub).
    pub gateway: String,
    /// Nameservers, one per line in the generated resolv.conf.
    pub dns: Vec<String>,
    /// Router-only—the enp0sN name the router should place the address on.
    pub iface: Option<String>,
}

/// Read `/opt/cargo-bay/external-net`. `None` when the file is absent
/// (`lan` mode). Missing required fields yield `None` too—the caller falls
/// back to the DHCP path rather than crashing bring-up.
pub fn read_external_net() -> Option<ExternalNet> {
    let text = std::fs::read_to_string("/opt/cargo-bay/external-net").ok()?;
    let mut ip_cidr = String::new();
    let mut gateway = String::new();
    let mut dns: Vec<String> = Vec::new();
    let mut iface: Option<String> = None;
    for line in text.lines() {
        let mut it = line.split_whitespace();
        match it.next() {
            Some("ip") => {
                if let Some(v) = it.next() {
                    ip_cidr = v.to_string();
                }
            }
            Some("gateway") => {
                if let Some(v) = it.next() {
                    gateway = v.to_string();
                }
            }
            Some("dns") => dns = it.map(str::to_string).collect(),
            Some("iface") => iface = it.next().map(str::to_string),
            _ => {}
        }
    }

    if ip_cidr.is_empty() || gateway.is_empty() {
        return None;
    }
    Some(ExternalNet { ip_cidr, gateway, dns, iface })
}

/// The host's per-node staging dir, mounted read-only in every guest.
pub const CARGO_BAY: &str = "/opt/cargo-bay";

/// A progress line (mirrors the scripts' `echo [tag] ...`).
pub fn note(msg: impl AsRef<str>) {
    println!("[voxel-init] {}", msg.as_ref());
}

/// A non-fatal warning (mirrors the scripts' `echo WARN: ...`).
pub fn warn(msg: impl AsRef<str>) {
    println!("[voxel-init] WARN: {}", msg.as_ref());
}

/// Apply literal `(from, to)` substitutions to `path` in one rewrite. Both role
/// agents use it to relax sshd_config, where the patterns are the distro's
/// shipped lines, commented or not. A pattern that does not match is silently a
/// no-op, which is what keeps the per-distro pattern lists safe to over-specify.
pub fn replace_in_file(path: &str, subs: &[(&str, &str)]) {
    let mut text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => {
            warn(format!("read {path}: {e}"));
            return;
        }
    };
    for (from, to) in subs {
        text = text.replace(from, to);
    }
    if let Err(e) = std::fs::write(path, text) {
        warn(format!("write {path}: {e}"));
    }
}

/// Run a command with inherited stdio, echoing it first (the `set -x` effect).
/// Returns whether it succeeded; never panics—use for best-effort steps.
pub fn run(cmd: &str, args: &[&str]) -> bool {
    run_env(cmd, args, &[])
}

/// Like [`run`], but with extra env vars for the child only. Preferred over
/// `std::env::set_var` (unsafe in edition 2024; racy under `getenv` from any
/// concurrent thread or C library) whenever the value is just being handed to a
/// subprocess.
pub fn run_env(cmd: &str, args: &[&str], envs: &[(&str, &str)]) -> bool {
    println!("+ {cmd} {}", args.join(" "));
    let mut c = Command::new(cmd);
    c.args(args).envs(envs.iter().copied());
    match c.status() {
        Ok(s) => s.success(),
        Err(e) => {
            warn(format!("{cmd}: {e}"));
            false
        }
    }
}

/// Run a command silently (stdio to /dev/null), returning success. Mirrors the
/// scripts' `... >/dev/null 2>&1` probes (e.g. `dladm show-link`, `iptables -C`).
pub fn run_quiet(cmd: &str, args: &[&str]) -> bool {
    Command::new(cmd)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Capture a command's trimmed stdout, or `None` if it failed to spawn / exited
/// nonzero.
pub fn capture(cmd: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(cmd).args(args).output().ok()?;
    if out.status.success() {
        Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        None
    }
}

/// The static address staged as `builder-net` when the host built this image on
/// an isolated external segment (no DHCP server to lease from).
pub struct BuilderNet {
    pub cidr: String,
    pub gateway: String,
}

/// Read `<cidr> <gateway>` from the cargo-bay. `None` when the host used the
/// default LAN path and the builder just DHCPs.
pub fn builder_net() -> Option<BuilderNet> {
    let text = fs::read_to_string(format!("{CARGO_BAY}/builder-net")).ok()?;
    let mut parts = text.split_whitespace();
    let cidr = parts.next()?.to_string();
    let gateway = parts.next()?.to_string();
    Some(BuilderNet { cidr, gateway })
}

/// Address the external NIC and wait for DNS. Static in isolated mode, DHCP
/// on a LAN.
pub fn bring_up_network() -> Result<()> {
    // The builder has a single external NIC; find a vioif and address it.
    let ext_if = std::env::var("EXT_IF")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            capture("dladm", &["show-phys", "-o", "link", "-p"]).and_then(|o| {
                o.lines().find(|l| l.starts_with("vioif")).map(str::to_string)
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
                let leased = capture(
                    "ipadm",
                    &["show-addr", &addrobj, "-p", "-o", "addr"],
                )
                .is_some_and(|a| a.contains('/'));
                if leased {
                    break;
                }
                sleep2();
            }
        }
    }
    wait_for_dns("pkg.oxide.computer");

    Ok(())
}

/// Poll until `host` resolves, up to 30s. Resolution lags the lease.
pub fn wait_for_dns(host: &str) {
    note("waiting for DNS...");
    for _ in 0..15 {
        if run_quiet("getent", &["hosts", host]) {
            return;
        }
        sleep2();
    }
}

pub fn sleep2() {
    std::thread::sleep(std::time::Duration::from_secs(2));
}

/// Root login over ssh for `voxel host login` and `voxel builder login`.
/// illumos sshd ships different defaults from debian's and needs its own
/// config edits.
pub fn setup_ssh() {
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
