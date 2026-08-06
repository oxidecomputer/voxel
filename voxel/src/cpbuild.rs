//! `voxel image create` - the per-commit control-plane build. Replaces
//! `build-cp.sh` and `fetch-sidecar.sh`.
//!
//! TUF can't be used here: its control-plane.tar.gz carries only the service
//! zones, not the i86pc global-zone software (sled-agent/switch/opte/mgs), so
//! the GZ half has to be a real i86pc omicron build.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::imagebuild::{
    BakeOpts, BakeStep, InstallRole, bake, isolated_builder, repo_root, toolchain_bin,
};

/// sidecar-lite pinned rev. TODO repin to main once zl/multicast merges.
const OMICRON_URL: &str = "https://github.com/oxidecomputer/omicron";
const SIDECAR_LITE_REV_PINNED: &str = "6f3311e8acd7e7e95c167aab61188355a93afe72";
const SIDECAR_URL: &str =
    "https://buildomat.eng.oxide.computer/public/file/oxidecomputer/sidecar-lite/release";

pub(crate) struct CpBuild<'a> {
    /// Image label; the image is named `voxel-cp-<label>`.
    pub label: &'a str,
    /// The omicron checkout to build.
    pub omicron_src: PathBuf,
    /// `--src`: build the checkout in place, no clone or checkout, so a dev's
    /// working-tree edits are what gets built.
    pub as_is: bool,
    /// Commit/tag to check out when not `as_is`.
    pub commit: Option<&'a str>,
    /// Gimlet SP count baked into the build-time smf configs.
    pub gimlets: usize,
    pub dataset: &'a str,
    /// The active `voxel.toml`, when there is one. Supplies the external
    /// segment for an isolated builder and the config-rss schema check.
    pub cfg: Option<&'a voxel_config::VoxelConfig>,
}

/// `voxel image create`: resolve where the omicron source is and what the image
/// is called, then build. `--src <path>` builds that checkout as-is with
/// `commit` reinterpreted as an optional image label; otherwise the commit names
/// both the checkout under the build root and the image.
pub(crate) async fn create(
    commit: Option<&str>,
    src: Option<&Path>,
    dataset: &str,
    cfg: Option<&voxel_config::VoxelConfig>,
) -> Result<()> {
    let (label, omicron_src, as_is) = match src {
        Some(s) => {
            let s = s
                .canonicalize()
                .with_context(|| format!("resolve --src {}", s.display()))?;
            if !s.join("package-manifest.toml").exists() {
                bail!(
                    "{} doesn't look like an omicron checkout (no package-manifest.toml)",
                    s.display()
                );
            }
            let label = match commit {
                Some(l) => l.to_string(),
                None => crate::image::head_short_sha(&s)?,
            };
            (label, s, true)
        }
        None => {
            let commit = commit.context("a <COMMIT> is required (or pass --src <path>)")?;
            let build_root = crate::env_vars::BUILD_ROOT
                .or_else(|| format!("{}/voxel-builds", crate::env_vars::home()));
            let src = PathBuf::from(build_root).join(format!("omicron-{commit}"));
            (commit.to_string(), src, false)
        }
    };
    let gimlets = crate::env_vars::GIMLETS
        .get()
        .and_then(|g| g.parse().ok())
        .unwrap_or(4);
    create_cp(CpBuild {
        label: &label,
        omicron_src,
        as_is,
        commit: if as_is { None } else { Some(&label) },
        gimlets,
        dataset,
        cfg,
    })
    .await
}

pub(crate) async fn create_cp(b: CpBuild<'_>) -> Result<()> {
    if std::env::consts::OS != "solaris" && !cfg!(target_os = "illumos") {
        // Belt and braces: the omicron build and falcon are Helios-only.
        eprintln!("[voxel] warning: control-plane builds are Helios-only");
    }
    let root = repo_root()?;
    let voxel_image = root.join("voxel-image");
    let cargo_bay = voxel_image.join("cargo-bay/vbuild");
    let src = &b.omicron_src;
    let image_name = format!("voxel-cp-{}", b.label);

    // --- 1. clone + checkout --------------------------------------------------
    if b.as_is {
        if !src.exists() {
            bail!("--src {} not found", src.display());
        }
        eprintln!("[voxel] building in place: {}", src.display());
    } else {
        let commit = b.commit.context("a commit is required without --src")?;
        if !src.join(".git").exists() {
            if let Some(parent) = src.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            eprintln!("[voxel] cloning omicron -> {}", src.display());
            let repo = crate::env_vars::OMICRON_REPO.or(OMICRON_URL);
            run(
                Command::new("git").arg("clone").arg(&repo).arg(src),
                "git clone omicron",
            )?;
        }
        eprintln!("[voxel] checking out {commit}");
        // A fetch failure is tolerable: the commit may already be local.
        let _ = Command::new("git")
            .arg("-C")
            .arg(src)
            .args(["fetch", "--all", "--tags", "-q"])
            .status();
        run(
            Command::new("git")
                .arg("-C")
                .arg(src)
                .args(["checkout", "-q", commit]),
            "git checkout",
        )?;
    }

    voxel_config::omicron::apply_patches(src)
        .map_err(|e| anyhow::anyhow!("patch omicron source: {e}"))?;

    // Fail on config-rss schema drift HERE, before the ~30 minutes of build
    // below, rather than at bring-up. Uses a default config when there's no
    // voxel.toml: the top-level key set doesn't depend on the topology.
    let owned;
    let schema_cfg = match b.cfg {
        Some(c) => c,
        None => {
            owned = voxel_config::VoxelConfig::default();
            &owned
        }
    };
    crate::topo::check_rss_schema(schema_cfg, src)?;

    // --- 2. prerequisites + softnpu machinery --------------------------------
    eprintln!("[voxel] install_builder_prerequisites.sh -y");
    run(
        omicron_cmd(src, "./tools/install_builder_prerequisites.sh").arg("-y"),
        "install_builder_prerequisites",
    )?;
    eprintln!("[voxel] ci_download_softnpu_machinery");
    run(
        &mut omicron_cmd(src, "./tools/ci_download_softnpu_machinery"),
        "ci_download_softnpu_machinery",
    )?;

    // --- 3. build the package tools ------------------------------------------
    eprintln!("[voxel] cargo build --release omicron-package xtask xtask-downloader");
    run(
        omicron_cmd(src, toolchain_bin("cargo").to_str().unwrap_or("cargo")).args([
            "build",
            "--release",
            "-p",
            "omicron-package",
            "-p",
            "xtask",
            "-p",
            "xtask-downloader",
        ]),
        "cargo build omicron package tools",
    )?;

    // --- 4. render build-time smf configs ------------------------------------
    eprintln!("[voxel] rendering smf configs, gimlets={}", b.gimlets);
    crate::image::render_smf(src, b.gimlets, b.cfg)?;

    // --- 5. package the control plane ----------------------------------------
    // NB: `-p a4x2` is OMICRON's own package preset (a build target in its
    // package-manifest), NOT the removed a4x2 testbed crate. Leave it as-is.
    eprintln!("[voxel] omicron-package target create -p a4x2");
    run(
        omicron_cmd(src, "./target/release/omicron-package")
            .args(["-t", "default", "target", "create", "-p", "a4x2"]),
        "omicron-package target create",
    )?;
    eprintln!("[voxel] omicron-package package");
    run(
        omicron_cmd(src, "./target/release/omicron-package").arg("package"),
        "omicron-package package",
    )?;

    check_generated_configs(src, schema_cfg)?;

    // --- 6. fetch the SoftNPU sidecar ----------------------------------------
    fetch_sidecar(&voxel_image, &cargo_bay.join("sidecar"))?;

    // --- 7. stage the curated omicron dir into the builder cargo-bay ---------
    let stage = cargo_bay.join("omicron");
    eprintln!("[voxel] staging omicron build -> {}", stage.display());
    let _ = std::fs::remove_dir_all(&stage);
    std::fs::create_dir_all(&stage).with_context(|| format!("mkdir {}", stage.display()))?;
    let mut rsync = omicron_cmd(src, "rsync");
    rsync.args([
        "-a",
        "tools",
        "out",
        "smf",
        "package-manifest.toml",
        "target/release/omicron-package",
        "target/release/xtask",
        "target/release/xtask-downloader",
    ]);
    // out/ holds host-side downloads the image doesn't need; the zones are
    // already unpacked in-guest from the tarballs we do keep.
    for ex in [
        "out/downloads",
        "out/clickhouse",
        "out/cockroachdb",
        "out/dendrite-stub",
        "out/mgd",
        "out/transceiver-control",
        "out/console-assets",
    ] {
        rsync.arg("--exclude").arg(ex);
    }
    rsync.arg(format!("{}/", stage.display()));
    run(&mut rsync, "rsync omicron -> cargo-bay")?;

    // --- 7b. build + stage the in-guest agent --------------------------------
    // Native illumos build: this box is the gimlet's OS.
    crate::imagebuild::stage_native_agent(&root, &cargo_bay)?;

    // --- 8. bake -------------------------------------------------------------
    let (ext_if, builder_net) = isolated_builder(b.cfg.map(|c| &c.external))?;
    bake(BakeOpts {
        base_image: "helios-3.0",
        step: BakeStep::Install(InstallRole::ControlPlane),
        cargo_bay: &cargo_bay.display().to_string(),
        image_name: &image_name,
        dataset: b.dataset,
        deploy: "voxel_build",
        disk_gb: 100,
        mem_gb: 16,
        cores: 8,
        ext_interface: ext_if.as_deref(),
        builder_net: builder_net.as_deref(),
    })
    .await?;

    crate::imagebuild::report_built(&image_name, "image.cp", b.cfg.map(|c| c.image.cp_image()));
    Ok(())
}

/// Render voxel's sled-agent config and config-rss, then have the omicron just
/// built parse them. Runs after packaging, so the compile is incremental and
/// links artifacts that already exist.
///
/// This is the thorough half of the schema checking. `check_rss_schema` runs in
/// seconds at the start of the build and catches top-level config-rss renames;
/// this catches everything, but only once omicron is built.
fn check_generated_configs(src: &Path, cfg: &voxel_config::VoxelConfig) -> Result<()> {
    let examples = src.join("sled-agent/examples");
    std::fs::create_dir_all(&examples).with_context(|| format!("mkdir {}", examples.display()))?;
    std::fs::write(
        src.join(voxel_config::omicron::PROBE_PATH),
        voxel_config::omicron::PROBE_SRC,
    )
    .context("write the config probe")?;

    let out = src.join("out/voxel-config-check");
    std::fs::create_dir_all(&out).with_context(|| format!("mkdir {}", out.display()))?;

    // Rack 0 of the active topology, or the default one. Shape is what is under
    // test, and it does not vary with sled count.
    let (data_links, disks) = crate::topo::detect_sled_schema(cfg, Some(src));
    let sled = cfg
        .sleds()
        .first()
        .context("config describes no sleds")?
        .sled_config(cfg.topology.sleds, 2, data_links, disks)
        .render();
    let sled_path = out.join("config.toml");
    std::fs::write(&sled_path, sled).context("write probe sled config")?;

    let pools = cfg
        .image
        .service_pool_schema
        .unwrap_or_else(|| crate::topo::detect_service_pool_schema(Some(src)));
    let rss_path = out.join("config-rss.toml");
    std::fs::write(
        &rss_path,
        cfg.to_config_rss(0, pools)
            .map_err(|e| anyhow::anyhow!("render config-rss for the probe: {e}"))?,
    )
    .context("write probe config-rss")?;

    eprintln!("[voxel] validating generated configs against this omicron");
    let status = omicron_cmd(src, toolchain_bin("cargo").to_str().unwrap_or("cargo"))
        .args([
            "run",
            "--release",
            "-q",
            "-p",
            "omicron-sled-agent",
            "--example",
            "voxel-config-check",
            "--",
        ])
        .arg(&sled_path)
        .arg(&rss_path)
        .status()
        .context("run the config probe")?;
    if !status.success() {
        bail!(
            "generated config rejected by omicron's own parsers (see above). \
             The rendered pair is in {}. Update voxel-config, or set the matching \
             [image] schema override.",
            out.display()
        );
    }
    Ok(())
}

/// Fetch sidecar-lite's scadm + libsidecar_lite.so, via a rev-keyed cache, and
/// stage them for the image. The builder VM may not reach buildomat.eng - only
/// the host does - so this happens here rather than in-guest.
fn fetch_sidecar(voxel_image: &Path, dest: &Path) -> Result<()> {
    let rev = crate::env_vars::SIDECAR_LITE_REV.or(SIDECAR_LITE_REV_PINNED);
    let cache = voxel_image.join(format!(".sidecar-lite/{rev}"));
    std::fs::create_dir_all(&cache).with_context(|| format!("mkdir {}", cache.display()))?;
    std::fs::create_dir_all(dest).with_context(|| format!("mkdir {}", dest.display()))?;
    for artifact in ["scadm", "libsidecar_lite.so"] {
        let cached = cache.join(artifact);
        if std::fs::metadata(&cached)
            .map(|m| m.len() == 0)
            .unwrap_or(true)
        {
            eprintln!("[voxel] fetching {artifact} @ {rev}");
            run(
                Command::new("curl")
                    .args(["-sSfL", "--retry", "10", "-o"])
                    .arg(&cached)
                    .arg(format!("{SIDECAR_URL}/{rev}/{artifact}")),
                "fetch sidecar artifact",
            )?;
        }
        let _ = Command::new("chmod").arg("+x").arg(&cached).status();
        std::fs::copy(&cached, dest.join(artifact)).with_context(|| format!("stage {artifact}"))?;
    }
    eprintln!(
        "[voxel] staged scadm + libsidecar_lite.so -> {}",
        dest.display()
    );
    Ok(())
}

/// A command run inside the omicron checkout, with the PATH and RUSTFLAGS the
/// omicron build needs. `install_builder_prerequisites.sh` ci-downloads
/// cockroach/clickhouse/dpd into `out/` and then fails unless they are on PATH -
/// a check that passes in an interactive dev shell but not a fresh one - so
/// those go on up front.
fn omicron_cmd(src: &Path, program: &str) -> Command {
    let mut c = Command::new(program);
    c.current_dir(src);
    let s = src.display();
    let existing = crate::env_vars::PATH.or("");
    let home = crate::env_vars::home();
    c.env(
        "PATH",
        format!(
            "{s}/out/cockroachdb/bin:{s}/out/clickhouse:{s}/out/dendrite-stub/bin:\
             {home}/.cargo/bin:/opt/ooce/bin:{existing}"
        ),
    );
    if !crate::env_vars::RUSTFLAGS.is_set() {
        c.env("RUSTFLAGS", voxel_config::omicron::RUSTFLAGS);
    }
    c
}

fn run(cmd: &mut Command, what: &str) -> Result<()> {
    let status = cmd.status().with_context(|| format!("run {what}"))?;
    if !status.success() {
        bail!("{what} failed");
    }
    Ok(())
}
