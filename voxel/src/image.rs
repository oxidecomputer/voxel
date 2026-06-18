//! `voxel image` - list / create / export / import / rm image bundles, plus the
//! hidden `render-smf` build helper.

use anyhow::{anyhow, Context};
use std::fs;
use std::path::{Path, PathBuf};

use crate::ImageCmd;

/// The resolved falcon dataset (set by `resolve_falcon_env`; else `rpool/falcon`).
pub(crate) fn falcon_dataset() -> String {
    std::env::var("FALCON_DATASET").unwrap_or_else(|_| "rpool/falcon".into())
}

/// Fail before staging/booting if a configured image isn't present, with a
/// message that points at how to get one - rather than the cryptic zfs clone
/// error falcon throws mid-launch when it can't find `<dataset>/img/<name>@base`.
pub(crate) fn ensure_image(image: &str) -> anyhow::Result<()> {
    let dataset = falcon_dataset();
    let snap = format!("{dataset}/img/{image}@base");
    let present = std::process::Command::new("zfs")
        .args(["list", "-t", "snapshot", "-H", "-o", "name", &snap])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !present {
        return Err(anyhow!(
            "image '{image}' not found ({snap}) - build it with `voxel image create <commit>`, \
             or run `voxel image ls` to see what's available"
        ));
    }
    Ok(())
}

/// Single-quote a path for safe interpolation into a `bash -c` pipeline.
fn shell_quote(p: &Path) -> String {
    format!("'{}'", p.display().to_string().replace('\'', "'\\''"))
}

/// Locate `voxel-image/build-cp.sh`: `VOXEL_BUILD_CP` override, else relative to
/// the running binary (`<exe>/../../voxel-image/build-cp.sh`), else CWD.
fn build_cp_script() -> anyhow::Result<PathBuf> {
    if let Ok(p) = std::env::var("VOXEL_BUILD_CP") {
        return Ok(PathBuf::from(p));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let cand = dir.join("../../voxel-image/build-cp.sh");
            if cand.exists() {
                return Ok(cand);
            }
        }
    }
    let cwd = PathBuf::from("voxel-image/build-cp.sh");
    if cwd.exists() {
        return Ok(cwd);
    }
    Err(anyhow!(
        "can't find build-cp.sh - set VOXEL_BUILD_CP to its path"
    ))
}

pub(crate) fn cmd_image(cmd: &ImageCmd) -> anyhow::Result<()> {
    match cmd {
        ImageCmd::Ls => {
            // Image bundles are falcon base images at <dataset>/img/<name>@base.
            let dataset = falcon_dataset();
            let img = format!("{dataset}/img");
            let out = std::process::Command::new("zfs")
                .args(["list", "-H", "-o", "name", "-t", "snapshot", "-r", &img])
                .output()
                .map_err(|e| anyhow!("run zfs list: {e}"))?;
            if !out.status.success() {
                return Err(anyhow!(
                    "zfs list {img} failed - is FALCON_DATASET correct? ({})",
                    String::from_utf8_lossy(&out.stderr).trim()
                ));
            }
            println!("image bundles under {img}:");
            let text = String::from_utf8_lossy(&out.stdout);
            let mut found = false;
            for line in text.lines() {
                // <dataset>/img/<name>@base  ->  <name>
                if let Some((path, snap)) = line.rsplit_once('@') {
                    if let Some(name) = path.strip_prefix(&format!("{img}/")) {
                        if name.starts_with("voxel-") {
                            println!("  {name}  ({snap})");
                            found = true;
                        }
                    }
                }
            }
            if !found {
                println!("  (none - build one with `voxel image create <commit>`)");
            }
            Ok(())
        }
        ImageCmd::Create { commit } => {
            let script = build_cp_script()?;
            eprintln!("[voxel] building voxel-cp-{commit} via {}", script.display());
            let status = std::process::Command::new("bash")
                .arg(&script)
                .arg(commit)
                .status()
                .map_err(|e| anyhow!("run {}: {e}", script.display()))?;
            if !status.success() {
                return Err(anyhow!("build-cp.sh failed for commit {commit}"));
            }
            println!("built image voxel-cp-{commit}");
            Ok(())
        }
        ImageCmd::Export { name, out, raw } => {
            let dataset = falcon_dataset();
            let snap = format!("{dataset}/img/{name}@base");
            // Confirm the image exists before streaming.
            let exists = std::process::Command::new("zfs")
                .args(["list", "-t", "snapshot", &snap])
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);
            if !exists {
                return Err(anyhow!("no such image snapshot: {snap} (try `voxel image ls`)"));
            }
            let (default_out, pipe) = if *raw {
                // Portable raw disk image: dd the zvol through xz.
                let zvol = format!("/dev/zvol/rdsk/{dataset}/img/{name}");
                (format!("{name}.raw.xz"), format!("dd if={zvol} bs=1M status=none | xz -T0 -c"))
            } else {
                // ZFS-native stream (allocated blocks only): zfs send through zstd.
                (format!("{name}.zfs.zst"), format!("zfs send {snap} | zstd -T0 -c"))
            };
            let out = out.clone().unwrap_or_else(|| PathBuf::from(default_out));
            eprintln!("[voxel] exporting {snap} -> {}", out.display());
            let status = std::process::Command::new("bash")
                .arg("-c")
                .arg(format!("{pipe} > {}", shell_quote(&out)))
                .status()
                .map_err(|e| anyhow!("export: {e}"))?;
            if !status.success() {
                return Err(anyhow!("export failed (need {} on PATH)", if *raw { "xz" } else { "zstd" }));
            }
            println!("exported {}", out.display());
            Ok(())
        }
        ImageCmd::Import { file } => {
            let dataset = falcon_dataset();
            let fname = file
                .file_name()
                .and_then(|s| s.to_str())
                .ok_or_else(|| anyhow!("bad file path"))?;
            // Derive image name + decompressor from the extension.
            let (name, decomp) = if let Some(n) = fname.strip_suffix(".zfs.zst") {
                (n.to_string(), format!("zstd -dc {}", shell_quote(file)))
            } else if let Some(n) = fname.strip_suffix(".raw.xz") {
                return Err(anyhow!(
                    "raw import for {n} not wired here yet - use build-image.sh's \
                     streaming raw import (presized zvol). zfs streams (.zfs.zst) import directly."
                ));
            } else {
                return Err(anyhow!("unrecognized extension on {fname} (want .zfs.zst or .raw.xz)"));
            };
            let dst = format!("{dataset}/img/{name}");
            eprintln!("[voxel] importing {} -> {dst}", file.display());
            let status = std::process::Command::new("bash")
                .arg("-c")
                .arg(format!("{decomp} | zfs recv {dst}"))
                .status()
                .map_err(|e| anyhow!("import: {e}"))?;
            if !status.success() {
                return Err(anyhow!("import failed (need zstd + zfs; {dst} must not already exist)"));
            }
            println!("imported {dst}@base (use: voxel config set image.cp {name})");
            Ok(())
        }
        ImageCmd::Rm { name, yes } => {
            let dataset = falcon_dataset();
            let ds = format!("{dataset}/img/{name}");
            if !yes {
                eprint!("destroy image {ds} and its @base snapshot? [y/N] ");
                use std::io::Write as _;
                std::io::stderr().flush().ok();
                let mut line = String::new();
                std::io::stdin().read_line(&mut line).ok();
                if !matches!(line.trim(), "y" | "Y" | "yes") {
                    println!("aborted");
                    return Ok(());
                }
            }
            let status = std::process::Command::new("zfs")
                .args(["destroy", "-r", &ds])
                .status()
                .map_err(|e| anyhow!("zfs destroy: {e}"))?;
            if !status.success() {
                return Err(anyhow!("zfs destroy {ds} failed (in use, or no such image?)"));
            }
            println!("removed {ds}");
            Ok(())
        }
        ImageCmd::RenderSmf { omicron_root, gimlets } => {
            // Bake switch0 for `gimlets` sleds with scrimlets at the first + last
            // sled (the convention the default topology follows). The launch-time
            // topology must keep scrimlets at those indices for the baked switch0
            // to match the generated switch1.
            let scrimlets = [0usize, gimlets.saturating_sub(1)];
            // One SP fleet drives both the MGS port table and sp-sim's side, so
            // they agree by construction. Baked images use the sim backend.
            let fleet = voxel_config::sp::SpFleet::sim(*gimlets);
            let writes = [
                (
                    "smf/mgs-sim/config.toml",
                    voxel_config::mgs::switch_config(0, &fleet, &scrimlets),
                ),
                ("smf/sp-sim/config.toml", fleet.sp_sim_config()),
                (
                    "smf/sled-agent/non-gimlet/config.toml",
                    voxel_config::sled::SledAgentConfig::new(0, true).render(),
                ),
            ];
            for (rel, text) in writes {
                let path = omicron_root.join(rel);
                let dir = path.parent().expect("smf path has a parent");
                fs::create_dir_all(dir).with_context(|| format!("mkdir {}", dir.display()))?;
                fs::write(&path, text).with_context(|| format!("write {}", path.display()))?;
                println!("rendered {}", path.display());
            }
            Ok(())
        }
    }
}
