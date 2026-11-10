// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `voxel heading <rev> [engage]`: everything from a fresh install to a
//! commtest-verified `--emu` rack running omicron `<rev>`.
//!
//! The steps are the ones an operator would otherwise run by hand: the frr
//! and builder images, a `voxel-cp-<rev>` image built in the builder,
//! `image.cp` pointed at it, `launch --emu`, `status` and `commtest`. A step
//! whose result already exists is skipped, so `engage` resumes after a
//! failure and a second heading reuses the frr and builder images. Without
//! `engage` it only prints the plan.

use crate::builder::Builder;
use crate::commtest;
use crate::imagebuild;
use crate::rack::{self, LaunchOpts};
use anyhow::{Context, Result, anyhow, bail};
use camino::{Utf8Path, Utf8PathBuf};
use voxel_config::{VoxelConfig, config as vcfg};

pub(crate) struct Opts<'a> {
    pub config_path: &'a Utf8Path,
    pub name: &'a str,
    pub rev: &'a str,
    pub engage: bool,
}

/// What the course needs and which parts of it already exist.
struct Plan {
    frr: String,
    frr_ready: bool,
    builder: String,
    builder_ready: bool,
    cp: String,
    cp_ready: bool,
    cp_selected: bool,
    rack_up: bool,
}

impl Plan {
    fn new(cfg: &VoxelConfig, rev: &str) -> Self {
        let frr = cfg.image.frr_image();
        let builder = cfg.builder_image();
        let cp = cp_image(rev);
        Self {
            frr_ready: image_exists(&frr),
            builder_ready: image_exists(&builder),
            cp_ready: image_exists(&cp),
            cp_selected: cfg.image.cp_image() == cp,
            rack_up: rack_up(cfg),
            frr,
            builder,
            cp,
        }
    }

    fn print(&self, rev: &str) {
        let mark = |done: bool| if done { "done" } else { "todo" };
        println!("heading: omicron {rev}");
        println!("  [{}] frr image {}", mark(self.frr_ready), self.frr);
        println!(
            "  [{}] builder image {}",
            mark(self.builder_ready),
            self.builder
        );
        println!(
            "  [{}] cp image {} (built in the builder)",
            mark(self.cp_ready),
            self.cp
        );
        println!("  [{}] image.cp = {}", mark(self.cp_selected), self.cp);
        let launch = match (self.rack_up, self.cp_selected) {
            (false, _) => "todo",
            (true, true) => "done",
            (true, false) => "BLOCKED: a rack on another image is up",
        };
        println!("  [{launch}] launch --emu");
        println!("  [todo] status");
        println!("  [todo] commtest");
    }
}

pub(crate) async fn run(o: Opts<'_>) -> Result<()> {
    let rev = o.rev;
    check_rev(rev)?;
    let cfg = crate::load_config(o.config_path)?;
    let plan = Plan::new(&cfg, rev);
    plan.print(rev);
    if plan.rack_up && !plan.cp_selected {
        bail!(
            "a rack running {} is up; `voxel destroy` it first",
            cfg.image.cp_image()
        );
    }
    if !o.engage {
        println!("`voxel heading {rev} engage` to make it so");
        return Ok(());
    }

    // Image builds cd into image-build/; everything after them expects the
    // workdir (`.falcon`, `builder/`).
    let workdir = Utf8PathBuf::try_from(std::env::current_dir()?)
        .context("non-utf8 working directory")?;
    let back = || {
        std::env::set_current_dir(&workdir)
            .with_context(|| format!("cd {workdir}"))
    };
    let dataset = crate::image::falcon_dataset();
    let propolis = cfg.falcon.propolis_binary.as_deref();

    if !plan.frr_ready {
        step(&format!("building frr image {}", plan.frr));
        let version = label(&plan.frr, "voxel-frr-")?;
        imagebuild::create_frr(
            version,
            &dataset,
            Some(&cfg.external),
            propolis,
        )
        .await?;
        back()?;
    }
    if !plan.builder_ready {
        step(&format!("building builder image {}", plan.builder));
        let version = label(&plan.builder, "voxel-builder-")?;
        imagebuild::create_builder(
            version,
            &dataset,
            Some(&cfg.external),
            rev,
            propolis,
        )
        .await?;
        back()?;
    }
    if !plan.cp_ready {
        step(&format!("building {} in the builder", plan.cp));
        let b = Builder::new(cfg.clone())?;
        if b.is_up()? {
            // `up` is what checks out the rev, so cycle a running builder.
            b.down(false, false).await?;
        }
        b.up(Some(rev), false, false, false).await?;
        back()?;
        b.build(label(&plan.cp, "voxel-cp-")?).await?;
        back()?;
        b.down(false, false).await?;
        back()?;
    }
    if !plan.cp_selected {
        step(&format!("image.cp = {}", plan.cp));
        set_config(o.config_path, "image.cp", &plan.cp)?;
    }
    let cfg = crate::load_config(o.config_path)?;

    if plan.rack_up {
        step("rack already up on this image; skipping launch");
    } else {
        step("launching the rack (--emu)");
        rack::cmd_launch(
            &cfg,
            o.name,
            LaunchOpts {
                no_progress: false,
                no_route: false,
                emu: true,
                init_rss: false,
                sp_firmware: None,
            },
        )
        .await?;
        back()?;
    }

    step("status");
    rack::cmd_status(&cfg, o.name).await?;

    step("commtest");
    commtest::run(
        &cfg,
        commtest::Options {
            config_path: o.config_path,
            source: None,
            rack: 1,
            api_override: None,
            traffic: commtest::Traffic::Unicast,
            no_build: false,
            passthrough: &[],
        },
    )?;
    step(&format!("arrived: omicron {rev}"));
    Ok(())
}

fn step(what: &str) {
    eprintln!("[voxel] heading: {what}");
}

/// The cp image for a rev: `voxel-cp-<first 7>`, the shape `image.cp_commit`
/// reads back to find the checkout.
fn cp_image(rev: &str) -> String {
    format!("voxel-cp-{}", &rev[..rev.len().min(7)])
}

/// Commit shas only: the image is named by the sha, and a branch name would
/// silently keep its old image after the branch moves.
fn check_rev(rev: &str) -> Result<()> {
    if rev.len() < 7
        || rev.len() > 40
        || !rev.chars().all(|c| c.is_ascii_hexdigit())
    {
        bail!(
            "heading takes an omicron commit sha (7-40 hex digits), not {rev:?}"
        );
    }
    Ok(())
}

/// The `<label>` of an image named `<prefix><label>`, which is all the image
/// builders take.
fn label<'a>(image: &'a str, prefix: &str) -> Result<&'a str> {
    image.strip_prefix(prefix).filter(|l| !l.is_empty()).ok_or_else(|| {
        anyhow!(
            "image {image} is missing and its name is not {prefix}<label>, \
             so heading cannot build it; build it by hand or change the config"
        )
    })
}

fn image_exists(image: &str) -> bool {
    crate::image::ensure_image(image).is_ok()
}

/// Whether the rack's first sled is running in this workdir.
fn rack_up(cfg: &VoxelConfig) -> bool {
    cfg.sleds().first().is_some_and(|s| {
        imagebuild::propolis_alive(Utf8Path::new(".falcon"), &s.name)
    })
}

fn set_config(path: &Utf8Path, key: &str, value: &str) -> Result<()> {
    let text = crate::config_text(path)?;
    let updated = vcfg::set(&text, key, value).map_err(|e| anyhow!(e))?;
    std::fs::write(path, updated).with_context(|| format!("write {path}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn revs_and_names() {
        assert!(check_rev("5ce2b2c").is_ok());
        assert!(check_rev("5ce2b2c132de306765c8aad0df9d1cd0d55d4021").is_ok());
        assert!(check_rev("main").is_err());
        assert!(check_rev("5ce2b2").is_err());
        assert_eq!(
            cp_image("5ce2b2c132de306765c8aad0df9d1cd0d55d4021"),
            "voxel-cp-5ce2b2c"
        );
        assert_eq!(label("voxel-frr-proto", "voxel-frr-").unwrap(), "proto");
        assert!(label("my-frr", "voxel-frr-").is_err());
    }
}
