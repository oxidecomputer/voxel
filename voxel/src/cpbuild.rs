//! `voxel image create` - the per-commit control-plane build.
//!
//! TUF can't be used here: its control-plane.tar.gz carries only the service
//! zones, not the i86pc global-zone software (sled-agent/switch/opte/mgs), so
//! the GZ half has to be a real i86pc omicron build.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::imagebuild::{BakeOpts, bake, builder_network, repo_root, toolchain_bin};

/// sidecar-lite pinned rev. TODO repin to main once zl/multicast merges.
const SIDECAR_LITE_REV: &str = "6f3311e8acd7e7e95c167aab61188355a93afe72";
const SIDECAR_URL: &str =
    "https://buildomat.eng.oxide.computer/public/file/oxidecomputer/sidecar-lite/release";

/// Build flags matching the validated recipe for building omicron on Helios.
/// pg_config (libpq) comes from /opt/ooce/bin, added to PATH below.
const OMICRON_RUSTFLAGS: &str = "--cfg svcadm_autoclear \
     -C link-arg=-R/usr/platform/oxide/lib/amd64 \
     -C link-arg=-Wl,-znocompstrtab --cfg tokio_unstable";

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
    pub num_gimlets: usize,
    pub dataset: &'a str,
    pub external: Option<&'a voxel_config::External>,
}

/// The omicron sha voxel's own rack-init-config dependency is pinned to. Empty while
/// the dependency is a path dep. Set by build.rs from Cargo.lock.
const PINNED_OMICRON_REV: &str = env!("RACK_INIT_CONFIG_OMICRON_REV");

/// `voxel image create`: resolve where the omicron source is and what the image
/// is called, then build. `--src <path>` builds that checkout as-is with
/// `commit` reinterpreted as an optional image label; otherwise the commit names
/// both the checkout under the build root and the image, defaulting to the
/// omicron rev voxel itself is pinned to.
pub(crate) async fn create(
    commit: Option<&str>,
    src: Option<&Path>,
    dataset: &str,
    external: Option<&voxel_config::External>,
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
            let commit = match commit {
                Some(c) => c.to_string(),
                None if !PINNED_OMICRON_REV.is_empty() => {
                    eprintln!(
                        "[voxel] no commit given; using voxel's pinned omicron {PINNED_OMICRON_REV}"
                    );
                    PINNED_OMICRON_REV.to_string()
                }
                None => bail!("a <COMMIT> is required (or pass --src <path>)"),
            };
            if !PINNED_OMICRON_REV.is_empty() && !PINNED_OMICRON_REV.starts_with(&commit) {
                eprintln!(
                    "[voxel] WARN: {commit} is not the omicron rev voxel is pinned to \
                     ({PINNED_OMICRON_REV}); the generated config-rss may not match this image"
                );
            }
            let build_root = std::env::var("BUILD_ROOT").unwrap_or_else(|_| {
                format!(
                    "{}/voxel-builds",
                    std::env::var("HOME").unwrap_or_else(|_| "/root".into())
                )
            });
            let src = PathBuf::from(build_root).join(format!("omicron-{commit}"));
            (commit, src, false)
        }
    };
    let num_gimlets = std::env::var("NUM_GIMLETS")
        .ok()
        .and_then(|g| g.parse().ok())
        .unwrap_or(4);
    create_cp(CpBuild {
        label: &label,
        omicron_src,
        as_is,
        commit: if as_is { None } else { Some(&label) },
        num_gimlets,
        dataset,
        external,
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
        eprintln!(
            "[voxel] building {} as-is (--src; no clone/checkout)",
            src.display()
        );
    } else {
        let commit = b.commit.context("a commit is required without --src")?;
        if !src.join(".git").exists() {
            if let Some(parent) = src.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            eprintln!("[voxel] cloning omicron -> {}", src.display());
            let repo = std::env::var("OMICRON_REPO")
                .unwrap_or_else(|_| "https://github.com/oxidecomputer/omicron".into());
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
        // Drop leftover local edits so the pinned commit builds pristine.
        run(
            Command::new("git")
                .arg("-C")
                .arg(src)
                .args(["checkout", "-q", "--", "."]),
            "git restore tracked files",
        )?;
    }

    // --- 2. prerequisites + softnpu machinery --------------------------------
    eprintln!("[voxel] install_builder_prerequisites.sh -y");
    run(
        omicron_cmd(src, "./tools/install_builder_prerequisites.sh").arg("-y"),
        "install_builder_prerequisites",
    )?;
    eprintln!("[voxel] ci_download_softnpu_machinery (out/npuzone)");
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
    eprintln!(
        "[voxel] rendering build-time smf configs from voxel-config (gimlets={})",
        b.num_gimlets
    );
    crate::image::render_smf(src, b.num_gimlets)?;

    // --- 5. package the control plane ----------------------------------------
    // NB: `-p a4x2` is OMICRON's own package preset (a build target in its
    // package-manifest), NOT the removed a4x2 testbed crate. Leave it as-is.
    eprintln!("[voxel] omicron-package target create -p a4x2");
    run(
        omicron_cmd(src, "./target/release/omicron-package")
            .args(["-t", "default", "target", "create", "-p", "a4x2"]),
        "omicron-package target create",
    )?;
    eprintln!("[voxel] omicron-package package (~11 min)");
    run(
        omicron_cmd(src, "./target/release/omicron-package").arg("package"),
        "omicron-package package",
    )?;

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
    eprintln!("[voxel] building voxel-init (native illumos) for the gimlet image");
    run(
        Command::new(toolchain_bin("cargo"))
            .current_dir(&root)
            .args(["build", "-p", "voxel-init", "--release"]),
        "cargo build voxel-init",
    )?;
    let agent = cargo_bay.join("voxel-init");
    std::fs::copy(root.join("target/release/voxel-init"), &agent)
        .context("stage voxel-init into the cargo-bay")?;
    let _ = Command::new("chmod").arg("+x").arg(&agent).status();

    // --- 8. bake -------------------------------------------------------------
    let network = builder_network(b.external)?;
    bake(BakeOpts {
        base_image: "helios-3.0",
        role: Some("cp"),
        exec: None,
        cargo_bay: &cargo_bay.display().to_string(),
        image_name: &image_name,
        dataset: b.dataset,
        deploy: "voxel_build",
        disk_gb: 100,
        mem_gb: 16,
        cores: 8,
        network: &network,
    })
    .await?;

    println!("built image {image_name}");
    Ok(())
}

/// Fetch sidecar-lite's scadm + libsidecar_lite.so, via a rev-keyed cache, and
/// stage them for the image. The builder VM may not reach buildomat.eng - only
/// the host does - so this happens here rather than in-guest.
fn fetch_sidecar(voxel_image: &Path, dest: &Path) -> Result<()> {
    let rev = std::env::var("SIDECAR_LITE_REV").unwrap_or_else(|_| SIDECAR_LITE_REV.to_string());
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
    let existing = std::env::var("PATH").unwrap_or_default();
    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".into());
    c.env(
        "PATH",
        format!(
            "{s}/out/cockroachdb/bin:{s}/out/clickhouse:{s}/out/dendrite-stub/bin:\
             {home}/.cargo/bin:/opt/ooce/bin:{existing}"
        ),
    );
    if std::env::var("RUSTFLAGS").is_err() {
        c.env("RUSTFLAGS", OMICRON_RUSTFLAGS);
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
