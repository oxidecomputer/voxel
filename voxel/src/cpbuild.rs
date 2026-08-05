//! `voxel image create` - the per-commit control-plane build. Replaces
//! `build-cp.sh` and `fetch-sidecar.sh`.
//!
//! TUF can't be used here: its control-plane.tar.gz carries only the service
//! zones, not the i86pc global-zone software (sled-agent/switch/opte/mgs), so
//! the GZ half has to be a real i86pc omicron build.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::imagebuild::{BakeOpts, bake, isolated_builder, repo_root, toolchain_bin};

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
            let build_root = std::env::var("BUILD_ROOT").unwrap_or_else(|_| {
                format!(
                    "{}/voxel-builds",
                    std::env::var("HOME").unwrap_or_else(|_| "/root".into())
                )
            });
            let src = PathBuf::from(build_root).join(format!("omicron-{commit}"));
            (commit.to_string(), src, false)
        }
    };
    let gimlets = std::env::var("GIMLETS")
        .ok()
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
    }

    apply_patches(src)?;

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
        role: Some("cp"),
        exec: None,
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

/// The probe voxel drops into the omicron checkout. It calls the same two
/// parsers `sled-agent`'s own `main` calls at boot, so "valid" means exactly
/// "this sled-agent would accept it": nested fields, renamed variants, changed
/// types and the semantic checks `RackInitializeRequest`'s `TryFrom` runs
/// (non-empty pools, external DNS inside the pool ranges) are all covered.
///
/// It only deserializes. It names no field and constructs no omicron type, so
/// it compiles unchanged against any era. That is the difference from the
/// generator this replaced, whose every breakage was in construction.
///
/// It lands in `examples/`, which cargo discovers without a manifest entry, so
/// there is no workspace member to add and no lockfile churn.
const CONFIG_PROBE: &str = r#"//! Generated by voxel. Validates voxel's rendered
//! configs with this commit's own parsers. Safe to delete.

fn report(what: &str, e: &dyn std::error::Error) {
    eprintln!("{what} rejected by this omicron:");
    eprintln!("  {e}");
    let mut src = e.source();
    while let Some(e) = src {
        eprintln!("  caused by: {e}");
        src = e.source();
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let usage = "usage: voxel-config-check <config.toml> <config-rss.toml>";
    let sled = camino::Utf8PathBuf::from(args.next().expect(usage));
    let rss = camino::Utf8PathBuf::from(args.next().expect(usage));

    let mut bad = false;
    if let Err(e) = omicron_sled_agent::config::Config::from_file(&sled) {
        report("sled-agent config", &e);
        bad = true;
    }
    if let Err(e) = sled_agent_rack_setup::rack_initialize_request_from_file(&rss) {
        report("config-rss", &e);
        bad = true;
    }
    if bad {
        std::process::exit(1);
    }
    println!("ok: both configs parse");
}
"#;

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
    std::fs::write(examples.join("voxel-config-check.rs"), CONFIG_PROBE)
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

/// Replace the single occurrence of `anchor` in `path` with `replacement`.
/// No-op if `marker` is already present, so a re-run is idempotent. Requiring
/// exactly one match means an upstream rewrite fails the build here instead of
/// silently producing a mis-built image.
fn patch_file(path: &Path, marker: &str, anchor: &str, replacement: &str) -> Result<()> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("read {} to patch", path.display()))?;
    if text.contains(marker) {
        return Ok(());
    }
    let hits = text.matches(anchor).count();
    if hits != 1 {
        bail!(
            "{}: expected exactly 1 match of the patch anchor, found {hits}. \
             Omicron changed upstream; the patch needs updating.",
            path.display()
        );
    }
    std::fs::write(path, text.replace(anchor, replacement))
        .with_context(|| format!("write patched {}", path.display()))?;
    if !file_contains(path, marker) {
        bail!("{}: patch did not apply", path.display());
    }
    Ok(())
}

/// voxel's two omicron source patches, re-applied after every checkout (which
/// resets the tree).
fn apply_patches(src: &Path) -> Result<()> {
    // sled-hardware returns a Pc baseboard for i86pc sleds, but wicketd
    // correlates bootstrap addresses by matching the SP's Gimlet baseboard, so
    // every sled shows "bootstrap address UNKNOWN". Revision 2 matches the
    // emulated SP VPD.
    eprintln!("[voxel] patching sled-hardware parse_smbios_output: Pc -> Gimlet baseboard");
    patch_file(
        &src.join("sled-hardware/src/illumos/mod.rs"),
        "new_gimlet(serial_number, product, 2)",
        "Some(Baseboard::new_pc(serial_number, product))",
        "Some(Baseboard::new_gimlet(serial_number, product, 2))",
    )?;

    eprintln!("[voxel] patching nexus rack-init: add v6 block to the infra address lot");
    apply_nexus_infra_lot_patch(&src.join("nexus/src/app/rack.rs"))?;
    Ok(())
}

/// Nexus rack-init builds the "initial-infra" address lot as a single block from
/// `rack_network_config.infra_ip_first`/`last` and lot-validates every
/// switch-port address against it. In Static mode that lot is a finite v4 range
/// (the numbered /30 uplinks), so voxel's sidecar-interconnect ports (underlay,
/// v6 addrconf) can't reserve and handoff 400s with "address not in lot". BGP
/// mode already uses a v6 `::` lot, where the same addrconf ports reserve fine.
///
/// The replacement's indentation lands in real Rust source that must compile.
fn apply_nexus_infra_lot_patch(rack_rs: &Path) -> Result<()> {
    patch_file(
        rack_rs,
        "voxel: add a v6 block",
        "        let blocks = vec![ipv4_block];",
        "        // voxel: add a v6 block so Static-mode addrconf (interconnect) ports\n\
         \x20       // reserve in the infra lot; BGP mode already uses a v6 :: lot.\n\
         \x20       let mut blocks = vec![ipv4_block];\n\
         \x20       if first_address.is_ipv4() {\n\
         \x20           blocks.push(networking::AddressLotBlockCreate {\n\
         \x20               first_address: std::net::Ipv6Addr::UNSPECIFIED.into(),\n\
         \x20               last_address: std::net::Ipv6Addr::UNSPECIFIED.into(),\n\
         \x20           });\n\
         \x20       }",
    )
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

fn file_contains(path: &Path, needle: &str) -> bool {
    std::fs::read_to_string(path)
        .map(|s| s.contains(needle))
        .unwrap_or(false)
}

fn run(cmd: &mut Command, what: &str) -> Result<()> {
    let status = cmd.status().with_context(|| format!("run {what}"))?;
    if !status.success() {
        bail!("{what} failed");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str, body: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("voxel-patch-{name}"));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("rack.rs");
        std::fs::write(&f, body).unwrap();
        f
    }

    /// Byte for byte the text the python patch this replaced emitted.
    #[test]
    fn nexus_infra_lot_patch_matches_reference_output() {
        let f = scratch(
            "lot",
            "BEFORE\n        let blocks = vec![ipv4_block];\nAFTER\n",
        );
        apply_nexus_infra_lot_patch(&f).unwrap();
        let want = "BEFORE\n\
        \x20       // voxel: add a v6 block so Static-mode addrconf (interconnect) ports\n\
        \x20       // reserve in the infra lot; BGP mode already uses a v6 :: lot.\n\
        \x20       let mut blocks = vec![ipv4_block];\n\
        \x20       if first_address.is_ipv4() {\n\
        \x20           blocks.push(networking::AddressLotBlockCreate {\n\
        \x20               first_address: std::net::Ipv6Addr::UNSPECIFIED.into(),\n\
        \x20               last_address: std::net::Ipv6Addr::UNSPECIFIED.into(),\n\
        \x20           });\n\
        \x20       }\nAFTER\n";
        assert_eq!(std::fs::read_to_string(&f).unwrap(), want);

        // Re-applying is a no-op, so a re-checked-out tree doesn't double-patch.
        apply_nexus_infra_lot_patch(&f).unwrap();
        assert_eq!(std::fs::read_to_string(&f).unwrap(), want);
    }

    /// A vanished anchor must fail the build. A silent no-op would surface much
    /// later as a handoff 400.
    #[test]
    fn patch_fails_when_anchor_is_gone() {
        let f = scratch("missing", "nothing to anchor on\n");
        assert!(apply_nexus_infra_lot_patch(&f).is_err());
    }

    /// Two matches are ambiguous; patching either could be wrong.
    #[test]
    fn patch_fails_when_anchor_is_ambiguous() {
        let line = "        let blocks = vec![ipv4_block];\n";
        let f = scratch("dup", &format!("{line}{line}"));
        assert!(apply_nexus_infra_lot_patch(&f).is_err());
    }
}
