//! `voxel image create` - the per-commit control-plane build.
//!
//! Two source modes. From source: clone and build omicron, package, bake.
//! `--from-tuf <repo.zip>`: no omicron build at all. Zones and measurement
//! corpus come from the repo's artifacts (byte exact, so the reconfigurator
//! can noop-convert against the repo), and the global-zone software
//! (sled-agent, switch zone, propolis, maghemite) is lifted from the repo's
//! host OS phase 2 payload, whose /opt/oxide is the installed form of it all.
//! The only non-repo pieces are the softnpu dendrite (the repo's switch zone
//! carries the tofino ASIC build; the prebuilt softnpu dpd named by the
//! pinned package-manifest replaces it) and voxel's own agent.

use anyhow::{Context, Result, bail};
use camino::{Utf8Path, Utf8PathBuf};
use std::process::Command;

use crate::imagebuild::{
    BakeOpts, bake, builder_network, repo_root, toolchain_bin,
};

/// sidecar-lite pinned rev. TODO repin to main once zl/multicast merges.
const SIDECAR_LITE_REV: &str = "6f3311e8acd7e7e95c167aab61188355a93afe72";
const SIDECAR_URL: &str = "https://buildomat.eng.oxide.computer/public/file/oxidecomputer/sidecar-lite/release";

/// Build flags matching the validated recipe for building omicron on Helios.
/// pg_config (libpq) comes from /opt/ooce/bin, added to PATH below.
const OMICRON_RUSTFLAGS: &str = "--cfg svcadm_autoclear \
     -C link-arg=-R/usr/platform/oxide/lib/amd64 \
     -C link-arg=-Wl,-znocompstrtab --cfg tokio_unstable";

pub(crate) struct CpBuild<'a> {
    /// Image label; the image is named `voxel-cp-<label>`.
    pub label: &'a str,
    /// The omicron checkout to build.
    pub omicron_src: Utf8PathBuf,
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
    src: Option<&Utf8Path>,
    from_tuf: Option<&Utf8Path>,
    sled_agent: Option<&Utf8Path>,
    dataset: &str,
    external: Option<&voxel_config::External>,
) -> Result<()> {
    if let Some(repo) = from_tuf {
        if src.is_some() {
            bail!("--from-tuf and --src are mutually exclusive");
        }
        let t = crate::tufrepo::TufRepoSource::load(repo)?;
        eprintln!(
            "[voxel] TUF source {}: system version {}, omicron {}",
            t.path, t.system_version, t.commit
        );
        // Raw-file fetches (package-manifest, schema sources) need a full sha.
        let full_sha = match commit {
            Some(c) if c.len() == 40 && c.starts_with(&t.commit) => {
                c.to_string()
            }
            Some(c) => bail!(
                "commit {c} is not the repo's omicron rev {} as a full sha",
                t.commit
            ),
            None if PINNED_OMICRON_REV.starts_with(&t.commit) => {
                PINNED_OMICRON_REV.to_string()
            }
            None => bail!(
                "voxel is pinned to omicron {PINNED_OMICRON_REV} but the repo \
                 was built from {}; repin voxel or pass the repo's full \
                 40-char omicron sha as <COMMIT>",
                t.commit
            ),
        };
        let num_gimlets = std::env::var("NUM_GIMLETS")
            .ok()
            .and_then(|g| g.parse().ok())
            .unwrap_or(4);
        return create_cp_tuf(
            t,
            &full_sha,
            sled_agent,
            num_gimlets,
            dataset,
            external,
        )
        .await;
    }
    let (label, omicron_src, as_is) = match src {
        Some(s) => {
            let s = s
                .canonicalize_utf8()
                .with_context(|| format!("resolve --src {s}"))?;
            if !s.join("package-manifest.toml").exists() {
                bail!(
                    "{s} doesn't look like an omicron checkout (no package-manifest.toml)"
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
            if !PINNED_OMICRON_REV.is_empty()
                && !PINNED_OMICRON_REV.starts_with(&commit)
            {
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
            let src =
                Utf8PathBuf::from(build_root).join(format!("omicron-{commit}"));
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
    // Everything under vbuild is regenerated per run. Leftovers owned by
    // another user (an earlier pfexec run) fail every later write; fail once
    // here instead.
    if cargo_bay.exists() {
        std::fs::remove_dir_all(&cargo_bay).with_context(|| {
            format!(
                "clear {cargo_bay} (owned by another user? \
                 remove it with pfexec rm -rf)"
            )
        })?;
    }
    let src = &b.omicron_src;
    let image_name = format!("voxel-cp-{}", b.label);

    // --- 1. clone + checkout --------------------------------------------------
    if b.as_is {
        if !src.exists() {
            bail!("--src {src} not found");
        }
        eprintln!("[voxel] building --src checkout {src}");
    } else {
        let commit = b.commit.context("a commit is required without --src")?;
        if !src.join(".git").exists() {
            if let Some(parent) = src.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            eprintln!("[voxel] cloning omicron -> {src}");
            let repo = std::env::var("OMICRON_REPO").unwrap_or_else(|_| {
                "https://github.com/oxidecomputer/omicron".into()
            });
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
    eprintln!(
        "[voxel] cargo build --release omicron-package xtask xtask-downloader"
    );
    run(
        omicron_cmd(src, toolchain_bin("cargo").as_str()).args([
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
    eprintln!("[voxel] staging omicron build -> {stage}");
    std::fs::create_dir_all(&stage)
        .with_context(|| format!("mkdir {stage}"))?;
    let mut rsync = omicron_cmd(src, "rsync");
    // No owner/group: staging is content-only and chgrp fails for non-root.
    rsync.args([
        "-a",
        "--no-owner",
        "--no-group",
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
    rsync.arg(format!("{stage}/"));
    run(&mut rsync, "rsync omicron -> cargo-bay")?;

    // --- 7b. build + stage the in-guest agent --------------------------------
    // Native illumos build: this box is the gimlet's OS.
    eprintln!(
        "[voxel] building voxel-init (native illumos) for the gimlet image"
    );
    run(
        Command::new(toolchain_bin("cargo")).current_dir(&root).args([
            "build",
            "-p",
            "voxel-init",
            "--release",
        ]),
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
        cargo_bay: &cargo_bay,
        image_name: &image_name,
        dataset: b.dataset,
        deploy: "voxel_build",
        disk_gb: 100,
        mem_gb: 16,
        cores: 8,
        network: &network,
    })
    .await?;

    // Stamp the sled-agent config schema on the image so launch reads it from
    // the image itself instead of re-deriving it from a checkout.
    let (data_links, disks) = crate::topo::schema_from_checkout(src)
        .with_context(|| format!("read sled-agent config schema from {src}"))?;
    run(
        Command::new("zfs")
            .arg("set")
            .arg(format!(
                "{}={}",
                crate::topo::PROP_DATA_LINKS,
                data_links.as_str()
            ))
            .arg(format!("{}={}", crate::topo::PROP_DISKS, disks.as_str()))
            .arg(format!("{}/img/{image_name}", b.dataset)),
        "zfs set sled schema on the image",
    )?;

    println!("built image {image_name}");
    Ok(())
}

/// Fetch sidecar-lite's scadm + libsidecar_lite.so, via a rev-keyed cache, and
/// stage them for the image. The builder VM may not reach buildomat.eng - only
/// the host does - so this happens here rather than in-guest.
fn fetch_sidecar(voxel_image: &Utf8Path, dest: &Utf8Path) -> Result<()> {
    let rev = std::env::var("SIDECAR_LITE_REV")
        .unwrap_or_else(|_| SIDECAR_LITE_REV.to_string());
    let cache = voxel_image.join(format!(".sidecar-lite/{rev}"));
    std::fs::create_dir_all(&cache)
        .with_context(|| format!("mkdir {cache}"))?;
    std::fs::create_dir_all(dest).with_context(|| format!("mkdir {dest}"))?;
    for artifact in ["scadm", "libsidecar_lite.so"] {
        let cached = cache.join(artifact);
        if std::fs::metadata(&cached).map(|m| m.len() == 0).unwrap_or(true) {
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
        std::fs::copy(&cached, dest.join(artifact))
            .with_context(|| format!("stage {artifact}"))?;
    }
    eprintln!("[voxel] staged scadm + libsidecar_lite.so -> {dest}");
    Ok(())
}

/// `--from-tuf`: build the image from the repo's own artifacts with no
/// omicron compile. See the module doc for the sourcing.
async fn create_cp_tuf(
    t: crate::tufrepo::TufRepoSource,
    full_sha: &str,
    sled_agent: Option<&Utf8Path>,
    num_gimlets: usize,
    dataset: &str,
    external: Option<&voxel_config::External>,
) -> Result<()> {
    let root = repo_root()?;
    let voxel_image = root.join("voxel-image");
    let cargo_bay = voxel_image.join("cargo-bay/vbuild");
    if cargo_bay.exists() {
        std::fs::remove_dir_all(&cargo_bay).with_context(|| {
            format!(
                "clear {cargo_bay} (owned by another user? \
                 remove it with pfexec rm -rf)"
            )
        })?;
    }
    std::fs::create_dir_all(&cargo_bay)
        .with_context(|| format!("mkdir {cargo_bay}"))?;
    let image_name = format!("voxel-cp-{}-tuf", t.commit);

    // --- 1. zones + measurement corpus, byte exact from the repo -------------
    let zones = t.extract_zones_into(&cargo_bay.join("zones"))?;
    eprintln!("[voxel] extracted {} zones from {}", zones.len(), t.path);
    let n = t.extract_corpus_into(&cargo_bay.join("measurements"))?;
    eprintln!("[voxel] extracted {n} measurement corpus artifacts");

    // --- 2. global-zone software from the host phase 2 payload ---------------
    let host_cache = voxel_image.join(".tuf-host");
    std::fs::create_dir_all(&host_cache)
        .with_context(|| format!("mkdir {host_cache}"))?;
    let payload = host_cache.join(format!("phase2-{}.img", t.commit));
    if !payload.exists() {
        eprintln!("[voxel] extracting host phase 2 payload -> {payload}");
        let n = t.extract_host_phase2_payload(&payload)?;
        eprintln!("[voxel] phase 2 payload: {n} bytes");
    }
    stage_gz_from_phase2(
        &payload,
        &host_cache.join("mnt"),
        &cargo_bay.join("gz"),
    )?;

    // The standard-image sled-agent hardwires scrimlet = tofino ASIC at
    // compile time (bootstrap/pre_server.rs sled_mode_from_config); softnpu
    // scrimlets need a switch-softnpu build staged over the phase 2 one.
    if let Some(pkg) = sled_agent {
        let dir = cargo_bay.join("gz/sled-agent");
        std::fs::remove_dir_all(&dir)
            .with_context(|| format!("clear {dir}"))?;
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("mkdir {dir}"))?;
        let f =
            std::fs::File::open(pkg).with_context(|| format!("open {pkg}"))?;
        if pkg.as_str().ends_with(".gz") {
            tar::Archive::new(flate2::read::GzDecoder::new(f)).unpack(&dir)
        } else {
            tar::Archive::new(f).unpack(&dir)
        }
        .with_context(|| format!("unpack {pkg}"))?;
        std::fs::copy(
            dir.join("pkg/manifest.xml"),
            cargo_bay.join("gz/sled-agent.xml"),
        )
        .context("stage the softnpu sled-agent SMF manifest")?;
        eprintln!("[voxel] staged softnpu sled-agent from {pkg}");
    } else {
        eprintln!(
            "[voxel] WARN: no --sled-agent; the phase 2 sled-agent cannot \
             run softnpu scrimlets"
        );
    }

    // --- 3. pinned single files from the omicron repo ------------------------
    let manifest =
        fetch_omicron_raw(&voxel_image, full_sha, "package-manifest.toml")?;
    // The prerequisites script calls sibling tools scripts; stage its closure.
    const RUNNER_TOOLS: [&str; 4] = [
        "install_runner_prerequisites.sh",
        "install_opte.sh",
        "opte_version",
        "opte_version_override",
    ];
    let tools = cargo_bay.join("tools");
    std::fs::create_dir_all(&tools)
        .with_context(|| format!("mkdir {tools}"))?;
    for name in RUNNER_TOOLS {
        let fetched = fetch_omicron_raw(
            &voxel_image,
            full_sha,
            &format!("tools/{name}"),
        )?;
        std::fs::copy(&fetched, tools.join(name))
            .with_context(|| format!("stage tools/{name}"))?;
    }

    // --- 4. softnpu dendrite over the repo's ASIC switch zone ----------------
    let dendrite = fetch_dendrite_softnpu(&voxel_image, &manifest)?;
    let scrimlets = [0usize, num_gimlets.saturating_sub(1)];
    let fleet = voxel_config::sp::SpFleet::sim(num_gimlets);
    let mgs_config = voxel_config::mgs::switch_config(0, &fleet, &scrimlets);
    recompose_switch_zone(
        &cargo_bay.join("gz/switch.tar.gz"),
        &dendrite,
        &mgs_config,
    )?;

    // --- 5. sidecar-lite + the agent ------------------------------------------
    fetch_sidecar(&voxel_image, &cargo_bay.join("sidecar"))?;
    // TUF builds run no compiler: the agent is the prebuilt binary shipped
    // (and workspace-built) alongside voxel itself.
    let agent_src = find_voxel_init()?;
    eprintln!("[voxel] staging voxel-init from {agent_src}");
    let agent = cargo_bay.join("voxel-init");
    std::fs::copy(&agent_src, &agent)
        .with_context(|| format!("stage {agent_src} into the cargo-bay"))?;
    let _ = Command::new("chmod").arg("+x").arg(&agent).status();

    // --- 6. bake ---------------------------------------------------------------
    let network = builder_network(external)?;
    bake(BakeOpts {
        base_image: "helios-3.0",
        role: Some("cp"),
        exec: None,
        cargo_bay: &cargo_bay,
        image_name: &image_name,
        dataset,
        deploy: "voxel_build",
        disk_gb: 100,
        mem_gb: 16,
        cores: 8,
        network: &network,
    })
    .await?;

    // --- 7. stamps -------------------------------------------------------------
    // The schema markers live in two source files; fetch them into a
    // checkout-shaped cache so the detector reads them as usual.
    fetch_omicron_raw(&voxel_image, full_sha, "sled-agent/src/config.rs")?;
    fetch_omicron_raw(&voxel_image, full_sha, "sled-hardware/src/lib.rs")?;
    let schema_root = raw_cache(&voxel_image, full_sha);
    let (data_links, disks) =
        crate::topo::schema_from_checkout(&schema_root)
            .context("derive sled schema from the pinned omicron sources")?;
    run(
        Command::new("zfs")
            .arg("set")
            .arg(format!(
                "{}={}",
                crate::topo::PROP_DATA_LINKS,
                data_links.as_str()
            ))
            .arg(format!("{}={}", crate::topo::PROP_DISKS, disks.as_str()))
            .arg(format!(
                "{}={}",
                crate::topo::PROP_TUF_VERSION,
                t.system_version
            ))
            .arg(format!("{dataset}/img/{image_name}")),
        "zfs set schema + tuf version on the image",
    )?;

    println!("built image {image_name}");
    Ok(())
}

/// The prebuilt voxel-init to bake: $VOXEL_INIT, else the binary next to this
/// voxel executable. The workspace builds both together and shipped bundles
/// carry both, so the sibling is fresh by construction.
fn find_voxel_init() -> Result<Utf8PathBuf> {
    if let Ok(p) = std::env::var("VOXEL_INIT") {
        let p = Utf8PathBuf::from(p);
        if !p.exists() {
            bail!("$VOXEL_INIT={p} does not exist");
        }
        return Ok(p);
    }
    let exe = std::env::current_exe().context("resolve the voxel binary")?;
    if let Some(dir) = exe.parent() {
        let sibling = dir.join("voxel-init");
        if sibling.exists() {
            return Utf8PathBuf::try_from(sibling)
                .context("non-utf8 voxel-init path");
        }
    }
    bail!(
        "no voxel-init next to the voxel binary; build it \
         (cargo build --release -p voxel-init) or set $VOXEL_INIT"
    )
}

/// Mount the phase 2 pool (read only, renamed, under `mnt`) and copy its
/// /opt/oxide plus the sled-agent SMF manifest into `dest`. Runs under
/// pfexec, so lofi/zpool/mount are direct.
fn stage_gz_from_phase2(
    payload: &Utf8Path,
    mnt: &Utf8Path,
    dest: &Utf8Path,
) -> Result<()> {
    const POOL: &str = "voxeltuf";
    std::fs::create_dir_all(dest).with_context(|| format!("mkdir {dest}"))?;
    std::fs::create_dir_all(mnt.join("ramdisk"))
        .with_context(|| format!("mkdir {mnt}/ramdisk"))?;
    // Best effort teardown of a previous run.
    let _ = Command::new("umount").arg(mnt.join("ramdisk")).output();
    let _ = Command::new("zpool").args(["export", POOL]).output();

    let dev = {
        let out = Command::new("lofiadm")
            .args(["-a", payload.as_str()])
            .output()
            .context("run lofiadm -a")?;
        if out.status.success() {
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        } else {
            // Already attached from a previous run; look the device up.
            let out = Command::new("lofiadm")
                .arg(payload.as_str())
                .output()
                .context("run lofiadm lookup")?;
            if !out.status.success() {
                bail!("lofiadm -a {payload} failed");
            }
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        }
    };
    let result = (|| -> Result<()> {
        // The pool is named rpool inside the image; find its id on our lofi
        // device and import it renamed so it cannot collide with the host's.
        let scan = Command::new("zpool")
            .args(["import", "-d", "/dev/lofi"])
            .output()
            .context("run zpool import scan")?;
        let scan = String::from_utf8_lossy(&scan.stdout).to_string();
        let dev_name = dev.rsplit('/').next().unwrap_or(&dev);
        let mut guid = None;
        let mut current = None;
        for line in scan.lines() {
            let line = line.trim();
            if let Some(id) = line.strip_prefix("id: ") {
                current = Some(id.trim().to_string());
            }
            if line.starts_with(&format!("/dev/lofi/{dev_name}"))
                || line.starts_with(dev_name)
            {
                guid = current.clone();
            }
        }
        let guid = guid.with_context(|| {
            format!("no importable pool found on {dev} (scan:\n{scan})")
        })?;
        run(
            Command::new("zpool").args([
                "import",
                "-d",
                "/dev/lofi",
                "-o",
                "readonly=on",
                "-R",
                mnt.as_str(),
                &guid,
                POOL,
            ]),
            "zpool import phase 2 pool",
        )?;
        run(
            Command::new("mount").args([
                "-F",
                "zfs",
                &format!("{POOL}/ROOT/ramdisk"),
                mnt.join("ramdisk").as_str(),
            ]),
            "mount phase 2 ramdisk root",
        )?;
        let oxide = mnt.join("ramdisk/opt/oxide");
        for e in
            oxide.read_dir_utf8().with_context(|| format!("read {oxide}"))?
        {
            let e = e?;
            run(
                Command::new("cp").args([
                    "-rp",
                    e.path().as_str(),
                    dest.as_str(),
                ]),
                "copy phase 2 /opt/oxide entry",
            )?;
        }
        std::fs::copy(
            mnt.join("ramdisk/lib/svc/manifest/site/sled-agent.xml"),
            dest.join("sled-agent.xml"),
        )
        .context("copy sled-agent SMF manifest")?;
        Ok(())
    })();
    let _ = Command::new("umount").arg(mnt.join("ramdisk")).output();
    let _ = Command::new("zpool").args(["export", POOL]).output();
    let _ = Command::new("lofiadm").args(["-d", &dev]).output();
    result
}

const OMICRON_RAW_URL: &str =
    "https://raw.githubusercontent.com/oxidecomputer/omicron";
const BUILDOMAT_URL: &str =
    "https://buildomat.eng.oxide.computer/public/file/oxidecomputer";

fn raw_cache(voxel_image: &Utf8Path, sha: &str) -> Utf8PathBuf {
    voxel_image.join(format!(".omicron-raw/{sha}"))
}

/// Fetch one file from the omicron repo at the pinned sha, via a sha-keyed
/// cache. This is the only omicron source access in a TUF image build.
fn fetch_omicron_raw(
    voxel_image: &Utf8Path,
    sha: &str,
    rel: &str,
) -> Result<Utf8PathBuf> {
    let cached = raw_cache(voxel_image, sha).join(rel);
    if cached.exists() {
        return Ok(cached);
    }
    let dir = cached.parent().context("raw cache path has a parent")?;
    std::fs::create_dir_all(dir).with_context(|| format!("mkdir {dir}"))?;
    eprintln!("[voxel] fetching omicron:{rel} @ {sha}");
    run(
        Command::new("curl")
            .args(["-sSfL", "--retry", "5", "-o"])
            .arg(&cached)
            .arg(format!("{OMICRON_RAW_URL}/{sha}/{rel}")),
        "fetch omicron raw file",
    )?;
    Ok(cached)
}

/// Fetch the prebuilt softnpu dendrite zone the pinned package-manifest names
/// (repo dendrite, source.commit + sha256), via a rev-keyed cache.
fn fetch_dendrite_softnpu(
    voxel_image: &Utf8Path,
    manifest: &Utf8Path,
) -> Result<Utf8PathBuf> {
    let text = std::fs::read_to_string(manifest)
        .with_context(|| format!("read {manifest}"))?;
    let doc: toml::Table =
        text.parse().with_context(|| format!("parse {manifest}"))?;
    let pkg = doc
        .get("package")
        .and_then(|p| p.as_table())
        .and_then(|p| p.get("dendrite-softnpu"))
        .context("package-manifest has no dendrite-softnpu package")?;
    let get = |key: &str| {
        pkg.get("source").and_then(|s| s.get(key)).and_then(|v| v.as_str())
    };
    let commit =
        get("commit").context("dendrite-softnpu has no source.commit")?;
    let sha256 =
        get("sha256").context("dendrite-softnpu has no source.sha256")?;
    let cached = voxel_image
        .join(format!(".dendrite-softnpu/{commit}/dendrite-softnpu.tar.gz"));
    if !cached.exists() {
        let dir = cached.parent().context("cache path has a parent")?;
        std::fs::create_dir_all(dir).with_context(|| format!("mkdir {dir}"))?;
        eprintln!("[voxel] fetching dendrite-softnpu @ {commit}");
        run(
            Command::new("curl")
                .args(["-sSfL", "--retry", "10", "-o"])
                .arg(&cached)
                .arg(format!(
                    "{BUILDOMAT_URL}/dendrite/image/{commit}/dendrite-softnpu.tar.gz"
                )),
            "fetch dendrite-softnpu",
        )?;
    }
    let out = Command::new("digest")
        .args(["-a", "sha256", cached.as_str()])
        .output()
        .context("run digest -a sha256")?;
    let got = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if got != sha256 {
        let _ = std::fs::remove_file(&cached);
        bail!("dendrite-softnpu sha256 {got} != manifest {sha256}");
    }
    Ok(cached)
}

/// Rewrite the phase 2 switch zone for voxel: swap the tofino ASIC dendrite
/// subtrees for the softnpu prebuilt's, and bake voxel's switch0 MGS config
/// in place of the rack's. Everything else (mgs, wicketd, mgd, omdb) is kept
/// as shipped.
fn recompose_switch_zone(
    switch: &Utf8Path,
    dendrite: &Utf8Path,
    mgs_config: &str,
) -> Result<()> {
    const DENDRITE_TREES: [&str; 2] =
        ["root/opt/oxide/dendrite", "root/var/svc/manifest/site/dendrite"];
    const MGS_CONFIG: &str = "root/var/svc/manifest/site/mgs/config.toml";
    let staged = switch.with_extension("recomposed");
    {
        let out = std::fs::File::create(&staged)
            .with_context(|| format!("create {staged}"))?;
        let enc =
            flate2::write::GzEncoder::new(out, flate2::Compression::default());
        let mut builder = tar::Builder::new(enc);

        let in_dendrite = |p: &str| {
            DENDRITE_TREES.iter().any(|t| {
                p == *t || p.strip_prefix(t).is_some_and(|r| r.starts_with('/'))
            })
        };
        copy_tar_entries(switch, &mut builder, |p| {
            !in_dendrite(p) && p != MGS_CONFIG
        })?;
        copy_tar_entries(dendrite, &mut builder, in_dendrite)?;

        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Regular);
        header.set_mode(0o644);
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(0);
        header.set_size(mgs_config.len() as u64);
        header.set_cksum();
        builder
            .append_data(&mut header, MGS_CONFIG, mgs_config.as_bytes())
            .context("append switch0 MGS config")?;
        builder.into_inner().context("finish tar")?.finish()?;
    }
    std::fs::rename(&staged, switch)
        .with_context(|| format!("replace {switch}"))?;
    eprintln!(
        "[voxel] recomposed switch zone (softnpu dendrite, voxel MGS config)"
    );
    Ok(())
}

/// Copy the entries of a tar.gz whose resolved paths pass `keep`, preserving
/// long names and links.
fn copy_tar_entries<W: std::io::Write>(
    from: &Utf8Path,
    builder: &mut tar::Builder<W>,
    keep: impl Fn(&str) -> bool,
) -> Result<()> {
    let f =
        std::fs::File::open(from).with_context(|| format!("open {from}"))?;
    let mut ar = tar::Archive::new(flate2::read::GzDecoder::new(f));
    for entry in ar.entries().with_context(|| format!("read {from}"))? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        let Some(p) = path.to_str() else { continue };
        if !keep(p) {
            continue;
        }
        let mut header = entry.header().clone();
        if let Some(link) = entry.link_name()? {
            builder.append_link(&mut header, &path, &link)?;
        } else {
            builder.append_data(&mut header, &path, &mut entry)?;
        }
    }
    Ok(())
}

/// A command run inside the omicron checkout, with the PATH and RUSTFLAGS the
/// omicron build needs. `install_builder_prerequisites.sh` ci-downloads
/// cockroach/clickhouse/dpd into `out/` and then fails unless they are on PATH -
/// a check that passes in an interactive dev shell but not a fresh one - so
/// those go on up front.
fn omicron_cmd(src: &Utf8Path, program: &str) -> Command {
    let mut c = Command::new(program);
    c.current_dir(src);
    let s = src;
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
