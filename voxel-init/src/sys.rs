// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Tiny process/file helpers shared by the role agents. The shell scripts these
//! replace ran with `set -x` and (for the gimlet) deliberately not `set -e`:
//! every step is visible and best-effort steps log a warning instead of
//! aborting. Mirror that—`run`/`run_quiet` never panic and return success.

use std::fs;
use std::io;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;
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

/// Install the staged `root_authorized_keys` into the
/// `/root/.ssh/authorized_keys` path as a block between the
/// demarcated markers of `# voxel-managed-begin` and `# voxel-managed-end`.
///
/// Lines outside these markers are left unchanged, while an empty staged file
/// drops the block, and a missing one leaves the file untouched.
pub fn sync_authorized_keys(staged: &str) {
    sync_authorized_keys_in(Path::new(staged), Path::new("/root/.ssh"));
}

const MANAGED_BEGIN: &str = "# voxel-managed-begin";
const MANAGED_END: &str = "# voxel-managed-end";

/// Outcome of a sync against `/root/.ssh/authorized_keys`; the caller
/// can report it in one place.
enum KeySync {
    /// No staged file, or nothing to clear; the file was not opened for
    /// writing.
    Untouched,
    /// The merged content matched what was already on disk, so nothing was
    /// written.
    Unchanged,
    /// The demarcated managed block was dropped and nothing remained, so it
    /// was safe to remove the file, which we did.
    Cleared,
    /// The file was rewritten through a temp file and rename.
    Synced,
}

fn sync_authorized_keys_in(staged: impl AsRef<Path>, dir: impl AsRef<Path>) {
    match sync_authorized_keys_io(staged.as_ref(), dir.as_ref()) {
        Ok(KeySync::Untouched) => {}
        Ok(KeySync::Unchanged) => note("root authorized_keys up to date"),
        Ok(KeySync::Cleared) => note("cleared root authorized_keys"),
        Ok(KeySync::Synced) => note("synced root authorized_keys"),
        Err(e) => warn(format!("authorized_keys: {e}")),
    }
}

fn sync_authorized_keys_io(staged: &Path, dir: &Path) -> io::Result<KeySync> {
    let Some(keys) = read_if_present(staged)? else {
        return Ok(KeySync::Untouched);
    };
    let path = dir.join("authorized_keys");
    let existing = read_if_present(&path)?.unwrap_or_default();
    let out = merge_managed_block(&existing, &keys);

    if out.is_empty() {
        return remove_if_present(&path).map(|removed| {
            if removed { KeySync::Cleared } else { KeySync::Untouched }
        });
    }
    if out == existing {
        return Ok(KeySync::Unchanged);
    }

    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .and_then(|()| {
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
        })
        .map_err(|e| at(e, "mkdir", dir))?;

    let tmp = dir.join(".authorized_keys.voxel-tmp");
    write_private(&tmp, &out)
        .and_then(|()| fs::rename(&tmp, &path))
        .inspect_err(|_| {
            let _ = fs::remove_file(&tmp);
        })
        .map(|()| KeySync::Synced)
        .map_err(|e| at(e, "write", &path))
}

fn read_if_present(path: &Path) -> io::Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(at(e, "read", path)),
    }
}

fn remove_if_present(path: &Path) -> io::Result<bool> {
    match fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(at(e, "remove", path)),
    }
}

fn write_private(path: &Path, body: &str) -> io::Result<()> {
    use std::io::Write;
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(body.as_bytes())?;
    f.sync_all()?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

fn at(e: io::Error, what: &str, path: &Path) -> io::Error {
    io::Error::new(e.kind(), format!("{what} {}: {e}", path.display()))
}

fn merge_managed_block(existing: &str, staged: &str) -> String {
    let mut out = String::new();
    let mut in_block = false;
    for line in existing.lines() {
        let line = line.trim_end_matches('\r');
        if line.trim_end() == MANAGED_BEGIN {
            in_block = true;
            continue;
        }
        if line.trim_end() == MANAGED_END {
            in_block = false;
            continue;
        }
        if in_block {
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    while out.ends_with("\n\n") {
        out.pop();
    }

    let keys: Vec<&str> = staged
        .lines()
        .map(|line| line.trim_end_matches('\r'))
        .filter(|line| !line.is_empty())
        .collect();
    if keys.is_empty() {
        return if out.trim().is_empty() { String::new() } else { out };
    }

    if !out.is_empty() {
        out.push('\n');
    }
    out.push_str(MANAGED_BEGIN);
    out.push('\n');
    for key in keys {
        out.push_str(key);
        out.push('\n');
    }
    out.push_str(MANAGED_END);
    out.push('\n');
    out
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

#[cfg(test)]
mod tests {
    use super::*;

    fn mode_of(path: impl AsRef<Path>) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn sync_missing_staged_touches_nothing() {
        let tmp = camino_tempfile::tempdir().unwrap();
        let dir = tmp.path().join(".ssh");
        let missing = tmp.path().join("missing");

        sync_authorized_keys_in(&missing, &dir);
        assert!(!dir.exists());

        fs::create_dir(&dir).unwrap();
        let path = dir.join("authorized_keys");
        let keys = b"ssh-ed25519 AAA existing\n";
        fs::write(&path, keys).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();

        sync_authorized_keys_in(&missing, &dir);
        assert_eq!(fs::read(&path).unwrap(), keys);
        assert_eq!(mode_of(&path), 0o640);
    }

    #[test]
    fn sync_invalid_staged_preserves_file() {
        let tmp = camino_tempfile::tempdir().unwrap();
        let dir = tmp.path().join(".ssh");
        fs::create_dir(&dir).unwrap();
        let path = dir.join("authorized_keys");
        let keys = b"ssh-ed25519 AAA existing\n";
        fs::write(&path, keys).unwrap();
        let staged = tmp.path().join("staged");
        fs::write(&staged, b"\xff").unwrap();

        sync_authorized_keys_in(&staged, &dir);

        assert_eq!(fs::read(&path).unwrap(), keys);
    }

    #[test]
    fn sync_replaces_block_keeps_other_lines() {
        let tmp = camino_tempfile::tempdir().unwrap();
        let dir = tmp.path().join(".ssh");
        fs::create_dir(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        let path = dir.join("authorized_keys");
        fs::write(
            &path,
            b"ssh-ed25519 AAA manual\n# voxel-managed-begin\nssh-ed25519 AAA old\n# voxel-managed-end\n",
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        let staged = tmp.path().join("staged");
        fs::write(&staged, b"ssh-ed25519 AAA staged\r\n\r\n").unwrap();

        sync_authorized_keys_in(&staged, &dir);

        assert_eq!(
            fs::read(&path).unwrap(),
            b"ssh-ed25519 AAA manual\n\n# voxel-managed-begin\nssh-ed25519 AAA staged\n# voxel-managed-end\n"
        );
        assert_eq!(mode_of(&dir), 0o700);
        assert_eq!(mode_of(&path), 0o600);
        assert!(!dir.join(".authorized_keys.voxel-tmp").exists());
    }

    #[test]
    fn sync_creates_file_and_dir() {
        let tmp = camino_tempfile::tempdir().unwrap();
        let dir = tmp.path().join(".ssh");
        let staged = tmp.path().join("staged");
        fs::write(&staged, b"ssh-ed25519 AAA staged\n").unwrap();

        sync_authorized_keys_in(&staged, &dir);

        assert_eq!(
            fs::read(dir.join("authorized_keys")).unwrap(),
            b"# voxel-managed-begin\nssh-ed25519 AAA staged\n# voxel-managed-end\n"
        );
        assert_eq!(mode_of(&dir), 0o700);
    }

    #[test]
    fn sync_empty_staged_drops_block() {
        let tmp = camino_tempfile::tempdir().unwrap();
        let dir = tmp.path().join(".ssh");
        let staged = tmp.path().join("staged");
        let path = dir.join("authorized_keys");

        fs::write(&staged, b"").unwrap();
        sync_authorized_keys_in(&staged, &dir);
        assert!(!dir.exists());

        fs::write(&staged, b"ssh-ed25519 AAA staged\n").unwrap();
        sync_authorized_keys_in(&staged, &dir);
        assert!(path.exists());

        fs::write(&staged, b"").unwrap();
        sync_authorized_keys_in(&staged, &dir);
        assert!(!path.exists());

        fs::write(&path, b"ssh-ed25519 AAA manual\n").unwrap();
        fs::write(&staged, b"ssh-ed25519 AAA staged\n").unwrap();
        sync_authorized_keys_in(&staged, &dir);
        fs::write(&staged, b"").unwrap();
        sync_authorized_keys_in(&staged, &dir);
        assert_eq!(fs::read(&path).unwrap(), b"ssh-ed25519 AAA manual\n");
    }

    #[test]
    fn sync_unchanged_keeps_permissions() {
        let tmp = camino_tempfile::tempdir().unwrap();
        let dir = tmp.path().join(".ssh");
        fs::create_dir(&dir).unwrap();
        let path = dir.join("authorized_keys");
        let content = b"ssh-ed25519 AAA manual\n\n# voxel-managed-begin\nssh-ed25519 AAA staged\n# voxel-managed-end\n";
        fs::write(&path, content).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        let staged = tmp.path().join("staged");
        fs::write(&staged, b"ssh-ed25519 AAA staged\n").unwrap();

        sync_authorized_keys_in(&staged, &dir);

        assert_eq!(fs::read(&path).unwrap(), content);
        assert_eq!(mode_of(&path), 0o640);
    }

    #[test]
    fn merge_moves_middle_block_and_matches_padded_markers() {
        let merged = merge_managed_block(
            "a\n# voxel-managed-begin \nold\n# voxel-managed-end\t\nb",
            "new\n",
        );
        assert_eq!(
            merged,
            "a\nb\n\n# voxel-managed-begin\nnew\n# voxel-managed-end\n"
        );
        assert_eq!(merge_managed_block(&merged, "new\n"), merged);
        assert_eq!(
            merge_managed_block("manual  \r\n", "k\n"),
            "manual  \n\n# voxel-managed-begin\nk\n# voxel-managed-end\n"
        );
    }

    #[test]
    fn merge_edge_cases() {
        // A begin marker without an ending slurps up everything after it;
        // only voxel writes these markers, making a missing end part of
        // voxel's block.
        assert_eq!(
            merge_managed_block(
                "ssh-ed25519 AAA manual\n# voxel-managed-begin\nold\nafter\n",
                "ssh-ed25519 AAA new\n",
            ),
            "ssh-ed25519 AAA manual\n\n# voxel-managed-begin\nssh-ed25519 AAA new\n# voxel-managed-end\n"
        );
        // Nothing in, nothing out: empty, whitespace-only, and block-only files
        // all merge to "" so the caller removes the file.
        assert_eq!(merge_managed_block("", ""), "");
        assert_eq!(merge_managed_block("\n\n", ""), "");
        assert_eq!(
            merge_managed_block(
                "# voxel-managed-begin\nk\n# voxel-managed-end\n",
                ""
            ),
            ""
        );
    }
}
