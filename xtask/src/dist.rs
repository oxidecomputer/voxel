//! `cargo xtask dist`: build the release archive CI publishes.
//!
//! The output directory holds, each with a `<name>.sha256.txt` (bare hex):
//!
//! - `voxel`, `voxel-init`: native illumos release builds
//! - `voxel-init-linux`: the router image's agent, a static musl build
//! - `sp-emu`: the build pins.toml pins, verified against its published digest
//! - `voxel.tar.gz`: those four, unprefixed, for a one-step install
//!
//! and `voxel-up.sh`, the installer for this commit. CI runs this command
//! and publishes the directory as is.

use anyhow::{Context, Result, bail};
use camino::{Utf8Path, Utf8PathBuf};
use sha2::{Digest, Sha256};
use std::process::Command;

/// The binaries in the archive, in archive order.
const BINS: &[&str] = &["voxel", "voxel-init", "voxel-init-linux", "sp-emu"];

const MUSL: &str = "x86_64-unknown-linux-musl";

/// Self-contained static linking for the musl agent: no cross toolchain on
/// the host beyond the rustup target.
const MUSL_RUSTFLAGS: &str = "-C linker=rust-lld -C link-self-contained=yes";

const BUILDOMAT_URL: &str =
    "https://buildomat.eng.oxide.computer/public/file/oxidecomputer";

pub fn dist(out_dir: &Utf8Path, rev: Option<&str>) -> Result<()> {
    if !cfg!(target_os = "illumos") {
        bail!(
            "cargo xtask dist runs on a Helios host: voxel and voxel-init are \
             illumos binaries"
        );
    }
    let root = crate::workspace_root();
    let out = if out_dir.is_absolute() {
        out_dir.to_path_buf()
    } else {
        root.join(out_dir)
    };
    let rev = crate::voxel_up::resolve_rev(rev.unwrap_or("HEAD"))?;
    warn_if_dirty(root);

    std::fs::create_dir_all(&out).with_context(|| format!("mkdir {out}"))?;
    // Clear the previous run's outputs, so a failure part way never leaves a
    // stale file that looks like this run's.
    for name in BINS.iter().copied().chain(["voxel.tar.gz", "voxel-up.sh"]) {
        for f in [out.join(name), out.join(format!("{name}.sha256.txt"))] {
            let _ = std::fs::remove_file(f);
        }
    }

    step("native build");
    cargo(
        root,
        &["build", "--release", "-p", "voxel", "-p", "voxel-init"],
        None,
    )?;
    let release = root.join("target/release");
    install(&release.join("voxel"), &out.join("voxel"))?;
    install(&release.join("voxel-init"), &out.join("voxel-init"))?;

    step("router agent");
    run(Command::new("rustup").args(["target", "add", MUSL]))
        .context("install the musl target (is rustup on PATH?)")?;
    cargo(
        root,
        &["build", "--release", "-p", "voxel-init", "--target", MUSL],
        Some(MUSL_RUSTFLAGS),
    )?;
    install(
        &root.join("target").join(MUSL).join("release/voxel-init"),
        &out.join("voxel-init-linux"),
    )?;

    step("sp-emu");
    fetch_sp_emu(root, &out)?;

    step("digests");
    for name in BINS {
        write_digest(&out.join(name))?;
    }

    step("bundle");
    run(Command::new("tar")
        .current_dir(&out)
        .args(["czf", "voxel.tar.gz.part"])
        .args(BINS))?;
    std::fs::rename(out.join("voxel.tar.gz.part"), out.join("voxel.tar.gz"))
        .context("rename voxel.tar.gz")?;
    write_digest(&out.join("voxel.tar.gz"))?;

    step("installer");
    crate::voxel_up::write(
        &rev,
        crate::voxel_up::RELEASE_URL,
        &out.join("voxel-up.sh"),
    )?;

    eprintln!(
        "\ndist for voxel {rev} in {out}\n\
         install it here with: {out}/voxel-up.sh --archive {out}/voxel.tar.gz"
    );
    Ok(())
}

fn step(name: &str) {
    eprintln!("==> {name}");
}

/// An archive built from uncommitted changes is fine for local use, but the
/// rendered voxel-up.sh's default rev will not fetch what was built.
fn warn_if_dirty(root: &Utf8Path) {
    let dirty = Command::new("git")
        .current_dir(root)
        .args(["status", "--porcelain", "--untracked-files=no"])
        .output()
        .is_ok_and(|o| o.status.success() && !o.stdout.is_empty());
    if dirty {
        eprintln!(
            "warning: the tree has uncommitted changes; the archive includes \
             them, CI's build of HEAD will not"
        );
    }
}

fn cargo(
    root: &Utf8Path,
    args: &[&str],
    rustflags: Option<&str>,
) -> Result<()> {
    let bin = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let mut cmd = Command::new(bin);
    cmd.current_dir(root).args(args);
    if let Some(flags) = rustflags {
        cmd.env("RUSTFLAGS", flags);
    }
    run(&mut cmd)
}

fn run(cmd: &mut Command) -> Result<()> {
    let what = format!("{cmd:?}");
    let st = cmd.status().with_context(|| format!("spawn {what}"))?;
    if !st.success() {
        bail!("{what} failed ({st})");
    }
    Ok(())
}

fn install(src: &Utf8Path, dst: &Utf8Path) -> Result<()> {
    std::fs::copy(src, dst).with_context(|| format!("copy {src} to {dst}"))?;
    make_executable(dst)
}

pub fn make_executable(p: &Utf8Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755))
        .with_context(|| format!("chmod {p}"))
}

/// Download the sp-emu build pins.toml names and check it against the digest
/// its own CI published.
fn fetch_sp_emu(root: &Utf8Path, out: &Utf8Path) -> Result<()> {
    let text = std::fs::read_to_string(root.join("pins.toml"))
        .context("read pins.toml")?;
    let pins: toml::Table = text.parse().context("parse pins.toml")?;
    let field = |key: &str| -> Result<String> {
        pins.get("sp-emu")
            .and_then(|t| t.get(key))
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .with_context(|| format!("pins.toml [sp-emu] has no {key}"))
    };
    let (repo, series, rev, artifact) =
        (field("repo")?, field("series")?, field("rev")?, field("artifact")?);
    let url = format!("{BUILDOMAT_URL}/{repo}/{series}/{rev}/{artifact}");

    let part = out.join("sp-emu.part");
    let want_file = out.join("sp-emu.upstream.sha256.txt");
    curl(&url, &part)?;
    curl(&format!("{url}.sha256.txt"), &want_file)?;
    let want = std::fs::read_to_string(&want_file)?;
    let want = want.split_whitespace().next().unwrap_or_default().to_string();
    let _ = std::fs::remove_file(&want_file);
    let got = sha256(&part)?;
    if got != want {
        let _ = std::fs::remove_file(&part);
        bail!("sp-emu {rev}: sha256 {got} != published {want}");
    }
    std::fs::rename(&part, out.join("sp-emu")).context("rename sp-emu")?;
    make_executable(&out.join("sp-emu"))
}

fn curl(url: &str, dst: &Utf8Path) -> Result<()> {
    run(Command::new("curl")
        .args(["-sSfL", "--retry", "5", "-o"])
        .arg(dst)
        .arg(url))
    .with_context(|| format!("download {url}"))
}

fn sha256(p: &Utf8Path) -> Result<String> {
    let mut f = std::fs::File::open(p).with_context(|| format!("open {p}"))?;
    let mut h = Sha256::new();
    std::io::copy(&mut f, &mut h).with_context(|| format!("read {p}"))?;
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// `<file>.sha256.txt`, bare hex as illumos `digest -a sha256` prints it.
fn write_digest(p: &Utf8Path) -> Result<()> {
    let sum = sha256(p)?;
    let dst = Utf8PathBuf::from(format!("{p}.sha256.txt"));
    std::fs::write(&dst, format!("{sum}\n"))
        .with_context(|| format!("write {dst}"))
}
