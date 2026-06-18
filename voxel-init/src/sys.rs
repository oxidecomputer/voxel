//! Tiny process/file helpers shared by the role agents. The shell scripts these
//! replace ran with `set -x` and (for the gimlet) deliberately not `set -e`:
//! every step is visible and best-effort steps log a warning instead of
//! aborting. Mirror that - `run`/`run_quiet` never panic and return success.

use std::process::{Command, Stdio};

/// A progress line (mirrors the scripts' `echo [tag] ...`).
pub fn note(msg: impl AsRef<str>) {
    println!("[voxel-init] {}", msg.as_ref());
}

/// A non-fatal warning (mirrors the scripts' `echo WARN: ...`).
pub fn warn(msg: impl AsRef<str>) {
    println!("[voxel-init] WARN: {}", msg.as_ref());
}

/// Run a command with inherited stdio, echoing it first (the `set -x` effect).
/// Returns whether it succeeded; never panics - use for best-effort steps.
pub fn run(cmd: &str, args: &[&str]) -> bool {
    println!("+ {cmd} {}", args.join(" "));
    match Command::new(cmd).args(args).status() {
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
