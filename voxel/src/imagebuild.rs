//! `voxel image bake` - build an image by booting a one-node builder, running a
//! step inside it, and capturing its disk as a registered falcon base image.
//!
//! `image create`, `image create-frr` and `image patch` all build through here.

use crate::isolated_external;
use anyhow::{Context, Result, bail};
use camino::{Utf8Path, Utf8PathBuf};
use libfalcon::{Runner, unit::gb};
use std::process::Command;
use std::time::Duration;

/// The builder node name, which fixes the falcon workdir layout
/// (`.falcon/vbuild.pid`). The DEPLOYMENT name is a parameter instead, so
/// `image patch` can run its boot-modify-capture under `voxel_patch` without
/// colliding with an image build.
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
    pub cargo_bay: &'a Utf8Path,
    /// Registered image name; captured to `<dataset>/img/<name>@base`.
    pub image_name: &'a str,
    pub dataset: &'a str,
    /// falcon deployment name (`voxel_build`; `voxel_patch` for `image patch`).
    pub deploy: &'a str,
    pub disk_gb: u64,
    pub mem_gb: u64,
    pub cores: u8,
    /// How the builder reaches the package repos.
    pub network: &'a BuilderNetwork,
}

/// How the builder reaches the package repos.
#[derive(Default)]
pub(crate) struct BuilderNetwork {
    /// Host link to attach the builder's external NIC to. `None` uses falcon's
    /// default external interface.
    pub interface: Option<String>,
    /// `"<cidr> <gateway>"` staged as `builder-net` for the guest to apply.
    /// `None` where the segment has DHCP and the guest can lease an address.
    pub static_address: Option<String>,
}

/// Prepare the host side of an image build's network.
///
/// The builder normally DHCPs an external NIC. In isolated mode that network is
/// the voxel-managed segment, which runs no DHCP, so the segment is brought up
/// here and the builder gets the stub plus a static address derived from
/// `host_ip - 1`. In lan mode falcon's default link and DHCP already work, so
/// this is empty.
pub(crate) fn builder_network(external: Option<&voxel_config::External>) -> Result<BuilderNetwork> {
    let Some(x) = external.filter(|x| x.isolated()) else {
        return Ok(BuilderNetwork::default());
    };
    isolated_external::up(x, isolated_external::DryRun::No)
        .context("bringing up the isolated external segment for the builder")?;
    let address = x.builder_net().with_context(|| {
        format!(
            "cannot derive a usable isolated builder address below host_ip '{}' within \
             subnet '{}'; choose a host_ip at least two addresses above the subnet network",
            x.host_ip, x.subnet
        )
    })?;
    Ok(BuilderNetwork {
        interface: Some(isolated_external::STUB.to_string()),
        static_address: Some(address),
    })
}

/// Bring up the builder, install, quiesce, capture, tear down.
pub(crate) async fn bake(o: BakeOpts<'_>) -> Result<()> {
    if !o.cargo_bay.exists() {
        bail!("cargo-bay {} not found", o.cargo_bay);
    }

    stage_builder_net(o.cargo_bay, o.network.static_address.as_deref())?;

    // ★ The builder MUST NOT share a falcon workspace with a running rack.
    // falcon keeps per-node pid/uuid files in `.falcon/` relative to the cwd,
    // and tearing the builder down destroys that whole directory - so building
    // an image from voxel's workdir would wipe a live rack's pid files, orphan
    // its propolis processes, and leave its VNICs busy.
    let workspace = repo_root()?.join("voxel-image");
    std::env::set_current_dir(&workspace).with_context(|| format!("cd {workspace}"))?;

    let mut d = Runner::new(o.deploy);
    let node = d.node(NODE, o.base_image, o.cores, gb(o.mem_gb));
    d.reserve(node, o.disk_gb as usize);

    match o.network.interface.as_deref() {
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
        d.mount_linux(o.cargo_bay.as_str(), "/opt/cargo-bay", node)
    } else {
        d.mount(o.cargo_bay.as_str(), "/opt/cargo-bay", node)
    };
    mounted.map_err(|e| anyhow::anyhow!("mount cargo-bay ({}): {e}", o.cargo_bay))?;

    eprintln!(
        "[voxel] booting builder {}, role {}",
        o.base_image,
        o.role.unwrap_or("none")
    );
    d.launch()
        .await
        .map_err(|e| anyhow::anyhow!("launch builder: {e}"))?;

    // An agent role, or an arbitrary command, or neither (boot-only smoke test).
    // The cargo-bay arrives without the exec bit, so the agent is copied to
    // local disk before running.
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
/// guest, stage it, bake.
///
/// Cross-compiling needs voxel's own source tree and a Rust toolchain on the
/// host. That is the remaining host dependency in the way of a fully
/// self-contained `voxel`; embedding a prebuilt agent in the binary would close
/// it.
pub(crate) async fn create_frr(
    version: &str,
    dataset: &str,
    external: Option<&voxel_config::External>,
) -> Result<()> {
    let root = repo_root()?;
    let network = builder_network(external)?;
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
        .args([
            "build",
            "-p",
            "voxel-init",
            "--release",
            "--target",
            FRR_TARGET,
        ])
        .env("RUSTFLAGS", "-C linker=rust-lld -C link-self-contained=yes")
        .status()
        .context("run cargo build -p voxel-init")?;
    if !built.success() {
        bail!("cross-compiling voxel-init for {FRR_TARGET} failed");
    }

    std::fs::create_dir_all(&cargo_bay).with_context(|| format!("mkdir {cargo_bay}"))?;
    let agent_src = root.join(format!("target/{FRR_TARGET}/release/voxel-init"));
    let agent_dst = cargo_bay.join("voxel-init");
    std::fs::copy(&agent_src, &agent_dst).with_context(|| format!("stage agent {agent_src}"))?;
    // The 9p mount drops the exec bit in-guest anyway, but keep it host-side.
    let _ = Command::new("chmod").args(["+x"]).arg(&agent_dst).status();

    bake(BakeOpts {
        base_image: "debian-13.2",
        role: Some("frr"),
        exec: None,
        cargo_bay: &cargo_bay,
        image_name: &image_name,
        dataset,
        deploy: "voxel_build",
        disk_gb: 20,
        mem_gb: 16,
        cores: 8,
        network: &network,
    })
    .await?;
    println!("built image {image_name}");
    Ok(())
}

/// Resolve a rust toolchain binary. `image bake` runs under `pfexec` (zfs +
/// falcon need it), which drops `~/.cargo/bin` from PATH - the shell scripts
/// papered over this by prepending it. Prefer an explicit override, then the
/// rustup install, then PATH.
pub(crate) fn toolchain_bin(name: &str) -> Utf8PathBuf {
    if let Ok(p) = std::env::var(name.to_uppercase()) {
        return Utf8PathBuf::from(p);
    }
    for home in [std::env::var("HOME").ok(), Some("/root".into())]
        .into_iter()
        .flatten()
    {
        let candidate = Utf8PathBuf::from(home).join(".cargo/bin").join(name);
        if candidate.exists() {
            return candidate;
        }
    }
    Utf8PathBuf::from(name)
}

/// voxel's own source tree, derived the same way `locate_script` finds
/// `voxel-image/`: next to the binary, else the cwd.
pub(crate) fn repo_root() -> Result<Utf8PathBuf> {
    if let Ok(root) = std::env::var("VOXEL_REPO_ROOT") {
        return Ok(Utf8PathBuf::from(root));
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
        && let Ok(candidate) = Utf8PathBuf::try_from(dir.join("../.."))
        && candidate.join("voxel-image").exists()
    {
        return Ok(candidate);
    }
    if Utf8Path::new("voxel-image").exists() {
        return Ok(Utf8PathBuf::from("."));
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
fn stage_builder_net(cargo_bay: &Utf8Path, explicit: Option<&str>) -> Result<()> {
    let path = cargo_bay.join("builder-net");
    let from_env = std::env::var("VOXEL_BUILDER_NET").ok();
    match explicit.map(str::to_string).or(from_env) {
        Some(net) if !net.trim().is_empty() => {
            eprintln!("[voxel] staging builder-net ({net}) into {cargo_bay}");
            std::fs::write(&path, format!("{}\n", net.trim()))
                .with_context(|| format!("write {path}"))?;
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
    eprintln!("[voxel] clearing device-instance map, halting");
    let _ = d
        .exec(
            node,
            "pkill -x devfsadmd 2>/dev/null; rm -f /etc/path_to_inst; sync; sync; (sleep 1; halt) &",
        )
        .await;
    wait_for_propolis_exit(NODE, Duration::from_secs(25));
    eprintln!("[voxel] stopping hypervisor");
    hyperstop(NODE);
}

/// Give the halting guest time to flush, returning early if propolis exits on
/// its own.
///
/// Propolis usually outlives the guest's halt, so reaching the timeout is the
/// normal path, not a failure: `hyperstop` takes the VM down next either way.
fn wait_for_propolis_exit(name: &str, timeout: Duration) {
    let Some(pid) = read_pidfile(name) else {
        return;
    };
    eprintln!("[voxel] flushing, up to {}s", timeout.as_secs());
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        // Signal 0 tests for existence without delivering anything.
        let alive = Command::new("pfexec")
            .args(["kill", "-0", &pid])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !alive {
            return;
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    eprintln!(
        "[voxel] still running after {}s; forcing it down",
        timeout.as_secs()
    );
}

/// The falcon workdir for the current deployment. `bake` has already `cd`-ed
/// here, so this is always the builder's own workspace and never a rack's.
fn falcon_dir() -> &'static Utf8Path {
    Utf8Path::new(".falcon")
}

/// Read a node's propolis pid, rejecting anything that is not a plain number so
/// a stray or truncated pidfile cannot turn into a `kill` against something
/// unrelated.
fn read_pidfile(name: &str) -> Option<String> {
    let raw = std::fs::read_to_string(falcon_dir().join(format!("{name}.pid"))).ok()?;
    let pid = raw.trim();
    if pid.is_empty() || !pid.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some(pid.to_string())
}

/// falcon's `hyperstop`, which is private to its CLI: SIGKILL the propolis
/// process from the pidfile, then destroy the bhyve VM by instance uuid.
fn hyperstop(name: &str) {
    let dir = falcon_dir();
    let pidfile = dir.join(format!("{name}.pid"));
    if let Some(pid) = read_pidfile(name) {
        let _ = Command::new("pfexec")
            .args(["kill", "-9", &pid])
            .stderr(std::process::Stdio::null())
            .status();
    }
    let _ = std::fs::remove_file(&pidfile);
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

/// Capture the builder's disk as a registered falcon base image.
///
/// Streams into a staging dataset and renames it over the target only once the
/// stream has succeeded, so a failed capture leaves the existing image intact.
///
/// A FULL (not incremental) `zfs send` is self-contained, so the image carries
/// no dependency on the base it was cloned from; only allocated blocks move.
fn capture(dataset: &str, image_name: &str, deploy: &str) -> Result<()> {
    let node = Dataset::new(format!("{dataset}/topo/{deploy}/{NODE}"))?;
    let zvol = format!("/dev/zvol/rdsk/{}", node.as_str());
    if !Utf8Path::new(&zvol).exists() {
        bail!("node zvol not found at {zvol}");
    }
    let image = Dataset::new(format!("{dataset}/img/{image_name}"))?;
    // Not `voxel-`-prefixed, so `image ls` never offers a half-streamed image.
    let staging = Dataset::new(format!("{dataset}/img/incoming-{image_name}"))?;
    let replacing = exists(&image);

    if replacing {
        eprintln!("[voxel] overwriting voxel image {image_name}");
    }
    // Left behind by a capture that died mid-stream.
    destroy_recursive_if_present(staging.as_str())?;
    // Snapshot of the builder's own disk, which is torn down next.
    destroy_recursive_if_present(&node.snapshot("base"))?;

    eprintln!("[voxel] building temp image {}", staging.as_str());
    snapshot(&node, "base")?;
    send_recv(&node.snapshot("base"), &staging)?;

    if replacing {
        eprintln!("[voxel] build complete, overwriting image {image_name}");
        destroy_recursive_if_present(image.as_str())?;
    }
    rename(&staging, &image)
}

fn exists(ds: &Dataset) -> bool {
    zfs_exists(ds.as_str())
}

fn rename(from: &Dataset, to: &Dataset) -> Result<()> {
    let status = Command::new("pfexec")
        .args(["zfs", "rename", from.as_str(), to.as_str()])
        .status()
        .with_context(|| format!("run zfs rename {} {}", from.as_str(), to.as_str()))?;
    if !status.success() {
        bail!("zfs rename {} -> {} failed", from.as_str(), to.as_str());
    }
    Ok(())
}

/// A zfs dataset voxel is allowed to operate on, validated on construction.
///
/// Everything below runs under `pfexec` and two of them destroy recursively, so
/// the name is checked here rather than trusted at the call site. These paths are
/// built by interpolation from a dataset root and an image name; an empty or
/// truncated component would silently widen a `destroy -r` to the parent and take
/// every image with it.
struct Dataset(String);

impl Dataset {
    fn new(path: String) -> Result<Self> {
        let components: Vec<&str> = path.split('/').collect();
        if components.len() < 3
            || components.iter().any(|c| c.is_empty())
            || path.contains(|c: char| c.is_whitespace())
            || path.contains('@')
        {
            bail!(
                "refusing to operate on zfs dataset {path:?}: expected a \
                 <pool>/<...>/<name> path with no empty components"
            );
        }
        Ok(Self(path))
    }

    fn as_str(&self) -> &str {
        &self.0
    }

    fn snapshot(&self, name: &str) -> String {
        format!("{}@{name}", self.0)
    }
}

/// `zfs destroy -r`, skipped when the target is absent.
///
/// Checks first rather than matching stderr: zfs words absence differently for
/// datasets and snapshots, and any failure that does reach us is a real one
/// (a busy dataset, a dependent clone, a permission failure).
fn destroy_recursive_if_present(target: &str) -> Result<()> {
    if !zfs_exists(target) {
        return Ok(());
    }
    let out = Command::new("pfexec")
        .args(["zfs", "destroy", "-r", target])
        .output()
        .with_context(|| format!("run zfs destroy -r {target}"))?;
    if !out.status.success() {
        bail!(
            "zfs destroy -r {target} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Whether a dataset or snapshot is present.
fn zfs_exists(name: &str) -> bool {
    Command::new("zfs")
        .args(["list", "-H", "-o", "name", name])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn snapshot(ds: &Dataset, name: &str) -> Result<()> {
    let snap = ds.snapshot(name);
    let status = Command::new("pfexec")
        .args(["zfs", "snapshot", &snap])
        .status()
        .with_context(|| format!("run zfs snapshot {snap}"))?;
    if !status.success() {
        bail!("zfs snapshot {snap} failed");
    }
    Ok(())
}

/// Stream a snapshot into a new dataset. The send is FULL, not incremental, so
/// the result carries no dependency on the base it was cloned from; only
/// allocated blocks move.
fn send_recv(from_snapshot: &str, to: &Dataset) -> Result<()> {
    let send = Command::new("pfexec")
        .args(["zfs", "send", from_snapshot])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .context("spawn zfs send")?;
    let recv = Command::new("pfexec")
        .args(["zfs", "recv", to.as_str()])
        .stdin(
            send.stdout
                .ok_or_else(|| anyhow::anyhow!("zfs send stdout"))?,
        )
        .status()
        .context("run zfs recv")?;
    if !recv.success() {
        bail!("zfs send {from_snapshot} | zfs recv {} failed", to.as_str());
    }
    Ok(())
}
