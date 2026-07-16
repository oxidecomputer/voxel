//! Small cross-module helpers (shell quoting, locating the bundled
//! `voxel-image/*` build scripts).

use std::path::PathBuf;

/// Single-quote a string for safe interpolation into a shell command: wrap in
/// `'...'` with any embedded single quote escaped as `'\''`. Path callers pass
/// `&p.display().to_string()`.
pub(crate) fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Locate a `voxel-image/<rel>` helper script: the `env_var` override first, else
/// relative to the running binary (`<exe>/../../voxel-image/<rel>`), else
/// `voxel-image/<rel>` under the CWD. Errors point at `env_var`.
pub(crate) fn locate_script(env_var: &str, rel: &str) -> anyhow::Result<PathBuf> {
    if let Ok(p) = std::env::var(env_var) {
        return Ok(PathBuf::from(p));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let cand = dir.join(format!("../../voxel-image/{rel}"));
            if cand.exists() {
                return Ok(cand);
            }
        }
    }
    let cwd = PathBuf::from(format!("voxel-image/{rel}"));
    if cwd.exists() {
        return Ok(cwd);
    }
    Err(anyhow::anyhow!(
        "can't find {rel} - set {env_var} to its path"
    ))
}
