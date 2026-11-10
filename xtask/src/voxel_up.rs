//! `cargo xtask voxel-up`: render the voxel-up.sh installer.

use anyhow::{Context, Result, bail};
use camino::Utf8Path;
use std::process::Command;

/// Where the `build` job publishes its `release` series (see
/// .github/buildomat/jobs/build.sh). Artifacts live under `<this>/<sha>/`.
pub const RELEASE_URL: &str = "https://buildomat.eng.oxide.computer/public/file/oxidecomputer/voxel/release";

const TEMPLATE: &str = include_str!("voxel-up.sh");

/// Write the installer for `rev` to `out` (`-` for stdout).
pub fn write(rev: &str, base_url: &str, out: &Utf8Path) -> Result<()> {
    let script = render(rev, base_url.trim_end_matches('/'));
    if out == "-" {
        print!("{script}");
        return Ok(());
    }
    std::fs::write(out, &script).with_context(|| format!("write {out}"))?;
    crate::dist::make_executable(out)?;
    eprintln!("wrote {out} for voxel {rev}");
    Ok(())
}

fn render(rev: &str, base_url: &str) -> String {
    TEMPLATE.replace("@VOXEL_REV@", rev).replace("@VOXEL_BASE_URL@", base_url)
}

/// buildomat keys published files by the full commit sha, so a branch, tag or
/// short sha is resolved through git.
pub fn resolve_rev(rev: &str) -> Result<String> {
    if is_full_sha(rev) {
        return Ok(rev.to_ascii_lowercase());
    }
    let o = Command::new("git")
        .current_dir(crate::workspace_root())
        .args(["rev-parse", "--verify", &format!("{rev}^{{commit}}")])
        .output()
        .context("run git rev-parse")?;
    if !o.status.success() {
        bail!(
            "cannot resolve {rev} to a commit: {}",
            String::from_utf8_lossy(&o.stderr).trim()
        );
    }
    let sha = String::from_utf8(o.stdout).context("git output")?;
    let sha = sha.trim();
    if !is_full_sha(sha) {
        bail!("git rev-parse {rev} gave {sha:?}, not a full sha");
    }
    Ok(sha.to_string())
}

fn is_full_sha(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_fills_every_placeholder() {
        let s = render(&"a".repeat(40), RELEASE_URL);
        assert!(!s.contains("@VOXEL_"));
        assert!(s.contains(&format!("VOXEL_REV:-{}", "a".repeat(40))));
        assert!(s.contains(&format!("VOXEL_BASE_URL:-{RELEASE_URL}")));
    }
}
