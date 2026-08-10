//! Tiny process/file helpers shared by the role agents. The shell scripts these
//! replace ran with `set -x` and (for the gimlet) deliberately not `set -e`:
//! every step is visible and best-effort steps log a warning instead of
//! aborting. Mirror that—`run`/`run_quiet` never panic and return success.

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
