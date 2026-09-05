// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `voxel image` - list / create / export / import / rm image bundles, plus the
//! hidden `render-smf` build helper.

use anyhow::{Context, bail};
use camino::{Utf8Path, Utf8PathBuf};
use std::fs;

use crate::ImageCmd;
use crate::util::shell_quote;

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
        bail!(
            "image '{image}' not found ({snap}) - build it with `voxel image create <commit>`, \
             or run `voxel image ls` to see what's available"
        );
    }
    Ok(())
}

/// The checkout's short HEAD sha (the default `--src` image label).
pub(crate) fn head_short_sha(src: &Utf8Path) -> anyhow::Result<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(src)
        // Under pfexec the checkout is usually owned by the invoking user,
        // which git rejects as dubious ownership; the path is operator-given.
        .arg("-c")
        .arg(format!("safe.directory={src}"))
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .with_context(|| format!("run git in {}", src))?;
    if !out.status.success() {
        bail!(
            "git rev-parse HEAD failed in {src}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Render the build-time smf configs (mgs-sim, sp-sim, sled-agent) into an
/// omicron checkout. Bakes switch0 for `gimlets` sleds with scrimlets at the
/// first + last sled (the default topology's convention): the launch-time
/// topology must keep scrimlets at those indices for the baked switch0 to match
/// the generated switch1. One SP fleet drives both the MGS port table and
/// sp-sim's side, so they agree by construction; baked images use the sim
/// backend.
pub(crate) fn render_smf(
    omicron_root: &Utf8Path,
    gimlets: usize,
) -> anyhow::Result<()> {
    let scrimlets = [0usize, gimlets.saturating_sub(1)];
    let fleet = voxel_config::sp::SpFleet::sim(gimlets);
    // The baked config must speak the same schema as the omicron being built.
    let (data_links, disks) = crate::topo::schema_from_checkout(omicron_root)
        .with_context(|| {
        format!("read sled-agent config schema from {omicron_root}")
    })?;
    let writes = [
        (
            "smf/mgs-sim/config.toml",
            voxel_config::mgs::switch_config(0, &fleet, &scrimlets),
        ),
        ("smf/sp-sim/config.toml", fleet.sp_sim_config()),
        (
            "smf/sled-agent/non-gimlet/config.toml",
            voxel_config::sled::SledAgentConfig::new(
                0, true, data_links, disks,
            )
            .render(),
        ),
    ];
    for (rel, text) in writes {
        let path = omicron_root.join(rel);
        let dir = path.parent().expect("smf path has a parent");
        fs::create_dir_all(dir).with_context(|| format!("mkdir {}", dir))?;
        fs::write(&path, text).with_context(|| format!("write {}", path))?;
        println!("rendered {}", path);
    }
    Ok(())
}

pub(crate) fn cmd_image(
    cmd: &ImageCmd,
    active: Option<String>,
) -> anyhow::Result<()> {
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
                .context("run zfs list")?;
            if !out.status.success() {
                bail!(
                    "zfs list {img} failed - is FALCON_DATASET correct? ({})",
                    String::from_utf8_lossy(&out.stderr).trim()
                );
            }
            let text = String::from_utf8_lossy(&out.stdout);
            let prefix = format!("{img}/");
            // volume name (short) -> (used, creation).
            let mut meta: std::collections::HashMap<String, (String, String)> =
                std::collections::HashMap::new();
            let mut bundles: Vec<String> = Vec::new();
            for line in text.lines() {
                let mut f = line.split('\t');
                let (name, used, creation, ty) =
                    match (f.next(), f.next(), f.next(), f.next()) {
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
                        if let Some((path, "base")) = name.rsplit_once('@')
                            && let Some(short) = path.strip_prefix(&prefix)
                            && short.starts_with("voxel-")
                        {
                            bundles.push(short.to_string());
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
                        if let (Some(n), Some(c)) = (f.next(), f.next())
                            && let Some(short) = n.strip_prefix(&prefix)
                        {
                            m.insert(short.to_string(), c.parse().unwrap_or(0));
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
        ImageCmd::Create { .. } => {
            bail!("internal: `image create` is dispatched in main")
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
                bail!("no such image snapshot: {snap} (try `voxel image ls`)");
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
            let out =
                out.clone().unwrap_or_else(|| Utf8PathBuf::from(default_out));
            eprintln!("[voxel] exporting {snap} -> {}", out);
            let status = std::process::Command::new("bash")
                .arg("-c")
                .arg(format!("{pipe} > {}", shell_quote(out.as_str())))
                .status()
                .context("export")?;
            if !status.success() {
                bail!(
                    "export failed (need {} on PATH)",
                    if *raw { "xz" } else { "zstd" }
                );
            }
            println!("exported {}", out);
            Ok(())
        }
        ImageCmd::Import { file } => {
            let dataset = falcon_dataset();
            let fname = file.file_name().context("bad file path")?;
            // Derive image name + decompressor from the extension.
            let (name, decomp) = if let Some(n) = fname.strip_suffix(".zfs.zst")
            {
                (
                    n.to_string(),
                    format!("zstd -dc {}", shell_quote(file.as_str())),
                )
            } else if let Some(n) = fname.strip_suffix(".raw.xz") {
                bail!(
                    "raw import for {n} is not implemented (it needs a presized zvol). \
                     Export as a zfs stream instead: those (.zfs.zst) import directly."
                );
            } else {
                bail!(
                    "unrecognized extension on {fname} (want .zfs.zst or .raw.xz)"
                );
            };
            let dst = format!("{dataset}/img/{name}");
            eprintln!("[voxel] importing {} -> {dst}", file);
            let status = std::process::Command::new("bash")
                .arg("-c")
                .arg(format!("{decomp} | zfs recv {dst}"))
                .status()
                .context("import")?;
            if !status.success() {
                bail!(
                    "import failed (need zstd + zfs; {dst} must not already exist)"
                );
            }
            println!(
                "imported {dst}@base (use: voxel config set image.cp {name})"
            );
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
                .context("zfs destroy")?;
            if !status.success() {
                bail!("zfs destroy {ds} failed (in use, or no such image?)");
            }
            println!("removed {ds}");
            Ok(())
        }
        // `image patch` needs the loaded config (for the default source image),
        // so it's dispatched in `main` before delegating the rest here.
        ImageCmd::Patch { .. } => {
            bail!("internal: `image patch` is dispatched in main")
        }
        ImageCmd::Bake { .. } => {
            bail!("internal: `image bake` is dispatched in main")
        }
        ImageCmd::CreateFrr { .. } => {
            bail!("internal: `image create-frr` is dispatched in main")
        }
        ImageCmd::CreateBird { .. } => {
            bail!("internal: `image create-bird` is dispatched in main")
        }
        ImageCmd::RenderSmf { omicron_root, gimlets } => {
            render_smf(omicron_root, *gimlets)
        }
    }
}
