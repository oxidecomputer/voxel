//! `voxel image bake` - build an image by booting a one-node builder, running
//! the in-guest agent's install mode, and capturing the disk.
//!
//! Replaces `voxel-image/build-image.sh` plus the `voxel-image-builder` crate:
//! voxel owns the falcon topology directly instead of shelling to a script that
//! shells to another binary. The build scripts that still exist (build-cp.sh,
//! build-frr.sh) call this instead of build-image.sh.

use anyhow::{Context, Result, bail};
use libfalcon::{Runner, unit::gb};
use std::path::Path;
use std::process::Command;

/// The builder node name. `build-image.sh` used the same, so the falcon workdir
/// layout (`.falcon/vbuild.pid`) is unchanged. The DEPLOYMENT name is a
/// parameter: `image patch` runs its boot-modify-capture under `voxel_patch` so
/// it cannot collide with an image build.
const NODE: &str = "vbuild";

pub(crate) struct BakeOpts<'a> {
    /// Base image to boot: `helios-3.0`, `debian-13.2`, or an existing voxel
    /// image when re-baking one (`image patch`).
    pub base_image: &'a str,
    /// Agent install role (`cp` / `frr`).
    pub role: Option<&'a str>,
    /// An arbitrary in-guest command to run instead of an agent role, for
    /// boot-modify-capture (`image patch` places a component this way). With
    /// neither `role` nor `exec` the builder just boots, which smoke-tests that
    /// a captured image comes up with its payload intact.
    pub exec: Option<&'a str>,
    /// Host dir mounted at `/opt/cargo-bay` in the guest.
    pub cargo_bay: &'a str,
    /// Registered image name; captured to `<dataset>/img/<name>@base`.
    pub image_name: &'a str,
    pub dataset: &'a str,
    /// falcon deployment name (`voxel_build`; `voxel_patch` for `image patch`).
    pub deploy: &'a str,
    pub disk_gb: u64,
    pub mem_gb: u64,
    pub cores: u8,
    /// Host link the builder reaches the package repos through. `None` uses
    /// falcon's default external interface.
    pub ext_interface: Option<&'a str>,
}

/// Bring up the builder, install, quiesce, capture, tear down.
pub(crate) async fn bake(o: BakeOpts<'_>) -> Result<()> {
    if !Path::new(o.cargo_bay).exists() {
        bail!("cargo-bay {} not found", o.cargo_bay);
    }

    stage_builder_net(o.cargo_bay)?;

    let mut d = Runner::new(o.deploy);
    let node = d.node(NODE, o.base_image, o.cores, gb(o.mem_gb));
    d.reserve(node, o.disk_gb as usize);

    match o.ext_interface {
        Some(ifx) => d.ext_link(ifx, node),
        None => d
            .default_ext_link(node)
            .map_err(|e| anyhow::anyhow!("find default external interface: {e}"))?,
    }

    // illumos guests use mount(); linux guests need mount_linux() (the guest-side
    // share mechanism differs). Pick from the base image name.
    let is_linux = ["debian", "ubuntu", "linux"]
        .iter()
        .any(|p| o.base_image.starts_with(p));
    let mounted = if is_linux {
        d.mount_linux(o.cargo_bay, "/opt/cargo-bay", node)
    } else {
        d.mount(o.cargo_bay, "/opt/cargo-bay", node)
    };
    mounted.map_err(|e| anyhow::anyhow!("mount cargo-bay ({}): {e}", o.cargo_bay))?;

    eprintln!(
        "[voxel] booting builder ({}), installing role {}",
        o.base_image,
        o.role.unwrap_or("<none>")
    );
    d.launch()
        .await
        .map_err(|e| anyhow::anyhow!("launch builder: {e}"))?;

    // An agent role, or an arbitrary command, or neither (boot-only smoke test).
    // The 9p cargo-bay mount drops the exec bit, so the agent is copied to local
    // disk before running. (build-image.sh needed a shell shim for that; driving
    // the exec ourselves lets us inline it.)
    let step = match (o.role, o.exec) {
        (Some(role), _) => Some((
            format!("install --role {role}"),
            format!(
                "cp /opt/cargo-bay/voxel-init /tmp/voxel-init && chmod +x /tmp/voxel-init && \
                 /tmp/voxel-init install --role {role} 2>&1 | tee /tmp/install.log"
            ),
        )),
        (None, Some(cmd)) => Some((format!("exec {cmd}"), cmd.to_string())),
        (None, None) => None,
    };
    if let Some((label, cmd)) = step {
        d.exec(node, &cmd)
            .await
            .map_err(|e| anyhow::anyhow!("{label}: {e}"))?;

        // falcon's exec does NOT propagate the guest command's exit code, so
        // validate the marker's CONTENT - written only on a complete run.
        let marker = d
            .exec(node, "cat /var/voxel-image-ready")
            .await
            .unwrap_or_default();
        if !marker.contains("version=") {
            bail!("ready marker missing/empty; {label} did not complete");
        }
        eprintln!("[voxel] ready: {}", marker.trim());
    }

    quiesce(&d, node).await;
    capture(o.dataset, o.image_name, o.deploy)?;

    eprintln!("[voxel] destroying builder topology");
    let _ = d.destroy();
    eprintln!(
        "[voxel] registered base image {}/img/{}@base",
        o.dataset, o.image_name
    );
    Ok(())
}

/// The musl target the Debian router guest needs. Static, so the router gets one
/// self-contained binary with no glibc or dynamic-linking dependency.
const FRR_TARGET: &str = "x86_64-unknown-linux-musl";

/// Build a `voxel-frr` router image: cross-compile the agent for the Debian
/// guest, stage it, bake. Replaces `build-frr.sh`.
///
/// Cross-compiling needs voxel's own source tree and a Rust toolchain on the
/// host. That is unchanged from the script, but it is the remaining host
/// dependency in the way of a fully self-contained `voxel` - a prebuilt agent
/// embedded in the binary would close it.
pub(crate) async fn create_frr(version: &str, dataset: &str) -> Result<()> {
    let root = repo_root()?;
    let image_name = format!("voxel-frr-{version}");
    let cargo_bay = root.join("voxel-image/cargo-bay/vbuild-frr");

    eprintln!("[voxel] cross-compiling voxel-init for {FRR_TARGET} (static)");
    // Best-effort: already-installed targets exit nonzero on some rustup versions.
    let _ = Command::new(toolchain_bin("rustup"))
        .args(["target", "add", FRR_TARGET])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    let built = Command::new(toolchain_bin("cargo"))
        .current_dir(&root)
        .args(["build", "-p", "voxel-init", "--release", "--target", FRR_TARGET])
        .env(
            "RUSTFLAGS",
            "-C linker=rust-lld -C link-self-contained=yes",
        )
        .status()
        .context("run cargo build -p voxel-init")?;
    if !built.success() {
        bail!("cross-compiling voxel-init for {FRR_TARGET} failed");
    }

    std::fs::create_dir_all(&cargo_bay)
        .with_context(|| format!("mkdir {}", cargo_bay.display()))?;
    let agent_src = root.join(format!("target/{FRR_TARGET}/release/voxel-init"));
    let agent_dst = cargo_bay.join("voxel-init");
    std::fs::copy(&agent_src, &agent_dst)
        .with_context(|| format!("stage agent {}", agent_src.display()))?;
    // The 9p mount drops the exec bit in-guest anyway, but keep it host-side.
    let _ = Command::new("chmod").args(["+x"]).arg(&agent_dst).status();

    bake(BakeOpts {
        base_image: "debian-13.2",
        role: Some("frr"),
        exec: None,
        cargo_bay: &cargo_bay.display().to_string(),
        image_name: &image_name,
        dataset,
        deploy: "voxel_build",
        disk_gb: 20,
        mem_gb: 16,
        cores: 8,
        ext_interface: None,
    })
    .await?;
    println!("built image {image_name}");
    Ok(())
}

/// Resolve a rust toolchain binary. `image bake` runs under `pfexec` (zfs +
/// falcon need it), which drops `~/.cargo/bin` from PATH - the shell scripts
/// papered over this by prepending it. Prefer an explicit override, then the
/// rustup install, then PATH.
fn toolchain_bin(name: &str) -> std::path::PathBuf {
    if let Ok(p) = std::env::var(name.to_uppercase()) {
        return std::path::PathBuf::from(p);
    }
    for home in [std::env::var("HOME").ok(), Some("/root".into())]
        .into_iter()
        .flatten()
    {
        let candidate = std::path::PathBuf::from(home).join(".cargo/bin").join(name);
        if candidate.exists() {
            return candidate;
        }
    }
    std::path::PathBuf::from(name)
}

/// voxel's own source tree, derived the same way `locate_script` finds
/// `voxel-image/`: next to the binary, else the cwd.
fn repo_root() -> Result<std::path::PathBuf> {
    if let Ok(root) = std::env::var("VOXEL_REPO_ROOT") {
        return Ok(std::path::PathBuf::from(root));
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let candidate = dir.join("../..");
        if candidate.join("voxel-image").exists() {
            return Ok(candidate);
        }
    }
    if Path::new("voxel-image").exists() {
        return Ok(std::path::PathBuf::from("."));
    }
    bail!("can't find voxel's source tree - set VOXEL_REPO_ROOT")
}

/// Stage the builder's static address for an isolated-mode build, or clear a
/// stale one.
///
/// The isolated segment runs no DHCP server, so `voxel image create` passes
/// `VOXEL_BUILDER_NET="<cidr> <gw>"` and the in-guest installer applies it in
/// place of DHCP. Clearing matters just as much: the cargo-bay is reused across
/// builds, and a leftover `builder-net` makes a later LAN build apply the
/// isolated static address to its DHCP-serving VNIC, losing package and DNS
/// access.
fn stage_builder_net(cargo_bay: &str) -> Result<()> {
    let path = Path::new(cargo_bay).join("builder-net");
    match std::env::var("VOXEL_BUILDER_NET") {
        Ok(net) if !net.trim().is_empty() => {
            eprintln!("[voxel] staging builder-net ({net}) into {cargo_bay}");
            std::fs::write(&path, format!("{}\n", net.trim()))
                .with_context(|| format!("write {}", path.display()))?;
        }
        _ => {
            let _ = std::fs::remove_file(&path);
        }
    }
    Ok(())
}

/// Clear the device-instance map and halt cleanly, as the LAST thing before
/// capture. A snapshot image must not carry the build VM's NIC/PCI layout or
/// each deployment node mis-binds vioif instances (vioif0, the SoftNPU
/// pkt_source, goes missing and the switch zone won't boot). An absent map plus
/// first-boot regeneration fixes that. It must be a clean halt, not a SIGKILL:
/// propolis has to flush the removal to the zvol or the last write is lost.
/// devfsadmd is stopped first so it cannot re-create the map.
async fn quiesce(d: &Runner, node: libfalcon::NodeRef) {
    eprintln!("[voxel] clearing device-instance map + clean halt (flush to disk)");
    let _ = d
        .exec(
            node,
            "pkill -x devfsadmd 2>/dev/null; rm -f /etc/path_to_inst; sync; sync; (sleep 1; halt) &",
        )
        .await;
    eprintln!("[voxel] waiting for clean shutdown to flush...");
    std::thread::sleep(std::time::Duration::from_secs(25));
    eprintln!("[voxel] stopping hypervisor (cleanup)");
    hyperstop(NODE);
}

/// falcon's `hyperstop`, which is private to its CLI: SIGKILL the propolis
/// process from the pidfile, then destroy the bhyve VM by instance uuid.
fn hyperstop(name: &str) {
    let dir = Path::new(".falcon");
    let pidfile = dir.join(format!("{name}.pid"));
    if let Ok(pid) = std::fs::read_to_string(&pidfile) {
        let pid = pid.trim();
        if !pid.is_empty() {
            let _ = Command::new("pfexec")
                .args(["kill", "-9", pid])
                .stderr(std::process::Stdio::null())
                .status();
        }
        let _ = std::fs::remove_file(&pidfile);
    }
    // The clean halt above usually already tore the VM down, so bhyvectl's
    // "could not be opened" is the normal case, not an error worth printing.
    if let Ok(uuid) = std::fs::read_to_string(dir.join(format!("{name}.uuid"))) {
        let uuid = uuid.trim();
        if !uuid.is_empty() {
            let _ = Command::new("pfexec")
                .args(["bhyvectl", "--destroy", &format!("--vm={uuid}")])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        }
    }
}

/// Capture the builder's disk as a registered falcon base image. A FULL (not
/// incremental) `zfs send` is self-contained, so the image carries no dependency
/// on the base it was cloned from; only allocated blocks move.
fn capture(dataset: &str, image_name: &str, deploy: &str) -> Result<()> {
    let node_ds = format!("{dataset}/topo/{deploy}/{NODE}");
    let zvol = format!("/dev/zvol/rdsk/{node_ds}");
    if !Path::new(&zvol).exists() {
        bail!("node zvol not found at {zvol}");
    }
    let img_ds = format!("{dataset}/img/{image_name}");
    eprintln!("[voxel] capturing (zfs send/recv) {node_ds} -> {img_ds}@base");

    // Best-effort: a prior run's snapshot / image usually does NOT exist, so
    // silence these - their "does not exist" noise reads like a real failure.
    zfs_quiet(&["destroy", "-r", &format!("{node_ds}@base")]);
    zfs_quiet(&["destroy", "-r", &img_ds]);
    zfs(&["snapshot", &format!("{node_ds}@base")])
        .context("snapshot the builder disk")?;

    // `zfs send | zfs recv`, both under pfexec.
    let send = Command::new("pfexec")
        .args(["zfs", "send", &format!("{node_ds}@base")])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .context("spawn zfs send")?;
    let recv = Command::new("pfexec")
        .args(["zfs", "recv", &img_ds])
        .stdin(send.stdout.ok_or_else(|| anyhow::anyhow!("zfs send stdout"))?)
        .status()
        .context("run zfs recv")?;
    if !recv.success() {
        bail!("zfs send | zfs recv failed capturing {node_ds} -> {img_ds}");
    }
    Ok(())
}

/// A best-effort `zfs` call whose failure is expected and uninteresting.
fn zfs_quiet(args: &[&str]) {
    let _ = Command::new("pfexec")
        .arg("zfs")
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

fn zfs(args: &[&str]) -> Result<()> {
    let status = Command::new("pfexec")
        .arg("zfs")
        .args(args)
        .status()
        .with_context(|| format!("run zfs {}", args.join(" ")))?;
    if !status.success() {
        bail!("zfs {} failed", args.join(" "));
    }
    Ok(())
}
