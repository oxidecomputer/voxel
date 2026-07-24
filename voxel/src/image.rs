//! `voxel image` - list / create / export / import / rm image bundles, plus the
//! hidden `render-smf` build helper.

use anyhow::{anyhow, Context};
use std::fs;
use std::path::PathBuf;

use crate::util::{locate_script, shell_quote};
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

/// Locate `voxel-image/build-cp.sh` (the `--host-build` fallback):
/// `VOXEL_BUILD_CP` override, else relative to the running binary, else CWD.
fn build_cp_script() -> anyhow::Result<PathBuf> {
    locate_script("VOXEL_BUILD_CP", "build-cp.sh")
}

/// Locate `voxel-image/build-cp-vm.sh` (the default VM build driver).
fn build_cp_vm_script() -> anyhow::Result<PathBuf> {
    locate_script("VOXEL_BUILD_CP_VM", "build-cp-vm.sh")
}

/// Locate `voxel-image/build-builder.sh` (bakes the `voxel-builder` base image).
fn build_builder_script() -> anyhow::Result<PathBuf> {
    locate_script("VOXEL_BUILD_BUILDER", "build-builder.sh")
}

pub(crate) fn cmd_image(cmd: &ImageCmd, active: Option<String>) -> anyhow::Result<()> {
    match cmd {
        ImageCmd::Ls => {
            // Image bundles are falcon base images at <dataset>/img/<name>@base.
            // One `zfs list` covers both the volumes (for size) and their @base
            // snapshots (which mark a name as a real bundle); we join them into a
            // table: image, location (dataset path), size, omicron commit.
            let dataset = falcon_dataset();
            let img = format!("{dataset}/img");
            let out = std::process::Command::new("zfs")
                .args([
                    "list",
                    "-H",
                    "-o",
                    "name,used,creation,type",
                    "-t",
                    "volume,snapshot",
                    "-r",
                    &img,
                ])
                .output()
                .map_err(|e| anyhow!("run zfs list: {e}"))?;
            if !out.status.success() {
                return Err(anyhow!(
                    "zfs list {img} failed - is FALCON_DATASET correct? ({})",
                    String::from_utf8_lossy(&out.stderr).trim()
                ));
            }
            let text = String::from_utf8_lossy(&out.stdout);
            let prefix = format!("{img}/");
            // volume name (short) -> (used, creation).
            let mut meta: std::collections::HashMap<String, (String, String)> =
                std::collections::HashMap::new();
            let mut bundles: Vec<String> = Vec::new();
            for line in text.lines() {
                let mut f = line.split('\t');
                let (name, used, creation, ty) = match (f.next(), f.next(), f.next(), f.next()) {
                    (Some(n), Some(u), Some(c), Some(t)) => (n, u, c, t),
                    _ => continue,
                };
                match ty {
                    "volume" => {
                        if let Some(short) = name.strip_prefix(&prefix) {
                            meta.insert(
                                short.to_string(),
                                (used.to_string(), creation.to_string()),
                            );
                        }
                    }
                    // A `<name>@base` snapshot is what makes <name> a bundle.
                    "snapshot" => {
                        if let Some((path, "base")) = name.rsplit_once('@') {
                            if let Some(short) = path.strip_prefix(&prefix) {
                                if short.starts_with("voxel-") {
                                    bundles.push(short.to_string());
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            // Order newest-first by creation. zfs's human `creation` string won't
            // sort, so pull the numeric epoch (`-p`) in a cheap second query keyed
            // by the same short names, and sort on that (name as the tiebreak).
            let epochs: std::collections::HashMap<String, i64> = {
                let mut m = std::collections::HashMap::new();
                if let Ok(o) = std::process::Command::new("zfs")
                    .args([
                        "list",
                        "-H",
                        "-p",
                        "-o",
                        "name,creation",
                        "-t",
                        "volume",
                        "-r",
                        &img,
                    ])
                    .output()
                {
                    for line in String::from_utf8_lossy(&o.stdout).lines() {
                        let mut f = line.split('\t');
                        if let (Some(n), Some(c)) = (f.next(), f.next()) {
                            if let Some(short) = n.strip_prefix(&prefix) {
                                m.insert(short.to_string(), c.parse().unwrap_or(0));
                            }
                        }
                    }
                }
                m
            };
            bundles.sort_by(|a, b| {
                let ea = epochs.get(a).copied().unwrap_or(0);
                let eb = epochs.get(b).copied().unwrap_or(0);
                eb.cmp(&ea).then_with(|| a.cmp(b))
            });
            bundles.dedup();
            if bundles.is_empty() {
                println!(
                    "no image bundles under {img} (build one with `voxel image create <commit>`)"
                );
                return Ok(());
            }
            // Commit from the name: voxel-cp-<commit>[-<variant>] -> <commit>.
            let commit_of = |name: &str| -> String {
                name.strip_prefix("voxel-cp-")
                    .and_then(|s| s.split('-').next())
                    .filter(|s| !s.is_empty())
                    .unwrap_or("-")
                    .to_string()
            };
            // The dataset is the same for every image, so name it once here and
            // drop the redundant per-row path; a leading marker flags image.cp.
            println!("images under {img}:\n");
            let active = active.as_deref();
            let mut any_active = false;
            let mut table: Vec<Vec<String>> = vec![vec![
                "".into(),
                "IMAGE".into(),
                "SIZE".into(),
                "CREATED".into(),
                "COMMIT".into(),
            ]];
            for name in &bundles {
                let is_active = active == Some(name.as_str());
                any_active |= is_active;
                let (used, created) = meta
                    .get(name)
                    .cloned()
                    .unwrap_or_else(|| ("-".into(), "-".into()));
                table.push(vec![
                    if is_active { "*".into() } else { "".into() },
                    name.clone(),
                    used,
                    created,
                    commit_of(name),
                ]);
            }
            let ncol = table[0].len();
            let mut w = vec![0usize; ncol];
            for row in &table {
                for (i, cell) in row.iter().enumerate() {
                    w[i] = w[i].max(cell.len());
                }
            }
            for row in &table {
                let line: String = row
                    .iter()
                    .enumerate()
                    .map(|(i, cell)| format!("{:<width$}", cell, width = w[i]))
                    .collect::<Vec<_>>()
                    .join("  ");
                println!("{}", line.trim_end());
            }
            if any_active {
                println!("\n* = current image.cp");
            }
            Ok(())
        }
        ImageCmd::Create {
            commit,
            persist_source,
            host_build,
        } => {
            // Default: build inside a `voxel-builder` VM (no host toolchain).
            // `--host-build`: legacy in-place host build.
            let script = if *host_build {
                build_cp_script()?
            } else {
                build_cp_vm_script()?
            };
            eprintln!(
                "[voxel] building voxel-cp-{commit} via {}",
                script.display()
            );
            let mut command = std::process::Command::new("bash");
            command.arg(&script).arg(commit);
            if *persist_source {
                // Honored by the VM path (keep source in image + VM up); the host
                // path always keeps its source on the box and ignores it.
                command.env("PERSIST_SOURCE", "1");
            }
            let status = command
                .status()
                .map_err(|e| anyhow!("run {}: {e}", script.display()))?;
            if !status.success() {
                return Err(anyhow!(
                    "{} failed for commit {commit}",
                    script.display()
                ));
            }
            println!("built image voxel-cp-{commit}");
            Ok(())
        }
        ImageCmd::BuilderCreate { force } => {
            let script = build_builder_script()?;
            eprintln!("[voxel] baking voxel-builder base image via {}", script.display());
            let mut command = std::process::Command::new("bash");
            command.arg(&script);
            if *force {
                command.env("FORCE", "1");
            }
            let status = command
                .status()
                .map_err(|e| anyhow!("run {}: {e}", script.display()))?;
            if !status.success() {
                return Err(anyhow!("build-builder.sh failed"));
            }
            println!("built base image voxel-builder");
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
                return Err(anyhow!(
                    "no such image snapshot: {snap} (try `voxel image ls`)"
                ));
            }
            let (default_out, pipe) = if *raw {
                // Portable raw disk image: dd the zvol through xz.
                let zvol = format!("/dev/zvol/rdsk/{dataset}/img/{name}");
                (
                    format!("{name}.raw.xz"),
                    format!("dd if={zvol} bs=1M status=none | xz -T0 -c"),
                )
            } else {
                // ZFS-native stream (allocated blocks only): zfs send through zstd.
                (
                    format!("{name}.zfs.zst"),
                    format!("zfs send {snap} | zstd -T0 -c"),
                )
            };
            let out = out.clone().unwrap_or_else(|| PathBuf::from(default_out));
            eprintln!("[voxel] exporting {snap} -> {}", out.display());
            let status = std::process::Command::new("bash")
                .arg("-c")
                .arg(format!(
                    "{pipe} > {}",
                    shell_quote(&out.display().to_string())
                ))
                .status()
                .map_err(|e| anyhow!("export: {e}"))?;
            if !status.success() {
                return Err(anyhow!(
                    "export failed (need {} on PATH)",
                    if *raw { "xz" } else { "zstd" }
                ));
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
                (
                    n.to_string(),
                    format!("zstd -dc {}", shell_quote(&file.display().to_string())),
                )
            } else if let Some(n) = fname.strip_suffix(".raw.xz") {
                return Err(anyhow!(
                    "raw import for {n} not wired here yet - use build-image.sh's \
                     streaming raw import (presized zvol). zfs streams (.zfs.zst) import directly."
                ));
            } else {
                return Err(anyhow!(
                    "unrecognized extension on {fname} (want .zfs.zst or .raw.xz)"
                ));
            };
            let dst = format!("{dataset}/img/{name}");
            eprintln!("[voxel] importing {} -> {dst}", file.display());
            let status = std::process::Command::new("bash")
                .arg("-c")
                .arg(format!("{decomp} | zfs recv {dst}"))
                .status()
                .map_err(|e| anyhow!("import: {e}"))?;
            if !status.success() {
                return Err(anyhow!(
                    "import failed (need zstd + zfs; {dst} must not already exist)"
                ));
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
                return Err(anyhow!(
                    "zfs destroy {ds} failed (in use, or no such image?)"
                ));
            }
            println!("removed {ds}");
            Ok(())
        }
        // `image patch` needs the loaded config (for the default source image),
        // so it's dispatched in `main` before delegating the rest here.
        ImageCmd::Patch { .. } => Err(anyhow!("internal: `image patch` is dispatched in main")),
        ImageCmd::RenderSmf {
            omicron_root,
            gimlets,
            out,
        } => {
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
            // `--out` writes under a staging dir (same `smf/...` layout, which the
            // guest copies into its checkout); else in-place under the checkout.
            let base = out.as_ref().unwrap_or(omicron_root);
            for (rel, text) in writes {
                let path = base.join(rel);
                let dir = path.parent().expect("smf path has a parent");
                fs::create_dir_all(dir).with_context(|| format!("mkdir {}", dir.display()))?;
                fs::write(&path, text).with_context(|| format!("write {}", path.display()))?;
                println!("rendered {}", path.display());
            }
            Ok(())
        }
    }
}
