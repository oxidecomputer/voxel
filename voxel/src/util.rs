// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Small cross-module helpers (shell quoting, locating the bundled
//! `voxel-image/*` build scripts).

use camino::Utf8PathBuf;

/// Single-quote a string for safe interpolation into a shell command: wrap in
/// `'...'` with any embedded single quote escaped as `'\''`. Path callers pass
/// `&p.to_string()`.
pub(crate) fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// The system temp dir as a UTF-8 path; panics on a non-UTF-8 TMPDIR.
pub(crate) fn temp_dir() -> Utf8PathBuf {
    Utf8PathBuf::try_from(std::env::temp_dir()).expect("TMPDIR is not UTF-8")
}

/// Locate a `voxel-image/<rel>` helper script: the `env_var` override first, else
/// relative to the running binary (`<exe>/../../voxel-image/<rel>`), else
/// `voxel-image/<rel>` under the CWD. Errors point at `env_var`.
pub(crate) fn locate_script(
    env_var: &str,
    rel: &str,
) -> anyhow::Result<Utf8PathBuf> {
    if let Ok(p) = std::env::var(env_var) {
        return Ok(Utf8PathBuf::from(p));
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
        && let Ok(candidate) =
            Utf8PathBuf::try_from(dir.join(format!("../../voxel-image/{rel}")))
        && candidate.exists()
    {
        return Ok(candidate);
    }
    let cwd = Utf8PathBuf::from(format!("voxel-image/{rel}"));
    if cwd.exists() {
        return Ok(cwd);
    }
    Err(anyhow::anyhow!("can't find {rel} - set {env_var} to its path"))
}
