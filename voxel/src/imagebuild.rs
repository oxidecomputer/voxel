// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Bake an image: boot a one-node builder, run the in-guest agent's install
//! role, and capture its disk as a registered falcon base image.
//!
//! `image create` and `image create-frr` both build through here. Everything
//! host-side (the builder's falcon dir, cargo-bays, download caches) lives
//! under `image-build/` in the anchored workdir, never in voxel's source tree.

use crate::isolated_external;
use crate::zfs::{self, Dataset};
use anyhow::{Context, Result, bail};
use camino::{Utf8Path, Utf8PathBuf};
use libfalcon::{Runner, unit::gb};
use std::process::Command;
use std::sync::OnceLock;
use std::time::Duration;

/// The builder node name, which fixes the falcon workdir layout
/// (`.falcon/vbuild.pid`).
const NODE: &str = "vbuild";
/// The builder's falcon deployment name.
const DEPLOY: &str = "voxel_build";

pub(crate) struct BakeOpts<'a> {
    /// Base image to boot: `helios-3.0` or `debian-13.2`.
    pub base_image: &'a str,
    /// Agent install role (`cp` / `frr`).
    pub role: &'a str,
    /// Host dir mounted at `/opt/cargo-bay` in the guest.
    pub cargo_bay: &'a Utf8Path,
    /// Registered image name; captured to `<dataset>/img/<name>@base`.
    pub image_name: &'a str,
    pub dataset: &'a str,
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

/// Host side of an image build's network. Isolated mode runs no DHCP: the
/// segment comes up here and the builder gets a static `host_ip - 1`.
pub(crate) fn builder_network(
    external: Option<&voxel_config::External>,
) -> Result<BuilderNetwork> {
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

    // The builder must not share a falcon workspace with a running rack:
    // teardown removes `.falcon/`, which holds the rack's pid files.
    let workspace = build_root()?;
    std::env::set_current_dir(&workspace)
        .with_context(|| format!("cd {workspace}"))?;

    let mut d = Runner::new(DEPLOY);
    let node = d.node(NODE, o.base_image, o.cores, gb(o.mem_gb));
    d.reserve(node, o.disk_gb as usize);

    match o.network.interface.as_deref() {
        Some(ifx) => d.ext_link(ifx, node),
        None => d.default_ext_link(node).map_err(|e| {
            anyhow::anyhow!("find default external interface: {e}")
        })?,
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
    mounted.map_err(|e| {
        anyhow::anyhow!("mount cargo-bay ({}): {e}", o.cargo_bay)
    })?;

    eprintln!(
        "[voxel] booting builder {}, role {}; image -> {}/img/{}",
        o.base_image, o.role, o.dataset, o.image_name
    );
    d.launch().await.map_err(|e| anyhow::anyhow!("launch builder: {e}"))?;

    // p9kp copies file by file with no integrity check. Confirm the agent
    // arrived whole before running it.
    let want = std::fs::metadata(o.cargo_bay.join("voxel-init"))
        .map(|m| m.len())
        .with_context(|| format!("stat {}/voxel-init", o.cargo_bay))?;
    let got = d
        .exec(node, "wc -c < /opt/cargo-bay/voxel-init")
        .await
        .map_err(|e| anyhow::anyhow!("size voxel-init in guest: {e}"))?;
    if got.trim() != want.to_string() {
        bail!(
            "cargo-bay copy incomplete in the builder: voxel-init is \
             {want} bytes on the host, guest reports {:?}",
            got.trim()
        );
    }

    // The cargo-bay arrives without the exec bit, so the agent is copied to
    // local disk before running.
    let label = format!("install --role {}", o.role);
    let cmd = format!(
        "cp /opt/cargo-bay/voxel-init /tmp/voxel-init && chmod +x /tmp/voxel-init && \
         /tmp/voxel-init install --role {} 2>&1 | tee /tmp/install.log",
        o.role
    );
    let console = d
        .exec(node, &cmd)
        .await
        .map_err(|e| anyhow::anyhow!("{label}: {e}"))?;

    // falcon's exec does NOT propagate the guest command's exit code, so
    // validate the marker's CONTENT - written only on a complete run.
    let marker =
        d.exec(node, "cat /var/voxel-image-ready").await.unwrap_or_default();
    if !marker.contains("version=") {
        // Surface the guest's own log before teardown destroys it.
        let tail = d
            .exec(node, "tail -40 /tmp/install.log 2>/dev/null")
            .await
            .unwrap_or_default();
        let last: Vec<&str> = console.lines().rev().take(40).collect();
        let console_tail =
            last.into_iter().rev().collect::<Vec<_>>().join("\n");
        if !console_tail.trim().is_empty() {
            eprintln!(
                "[voxel] guest console tail:\n{}",
                console_tail.trim_end()
            );
        }
        if tail.trim().is_empty() {
            bail!(
                "{label} wrote no /tmp/install.log; the agent never \
                 ran or died before producing output"
            );
        } else {
            eprintln!("[voxel] guest install log tail:\n{}", tail.trim_end());
        }
        bail!("ready marker missing/empty; {label} did not complete");
    }
    eprintln!("[voxel] ready: {}", marker.trim());

    quiesce(&d, node).await;
    capture(o.dataset, o.image_name)?;

    eprintln!("[voxel] destroying builder topology");
    let _ = d.destroy();
    eprintln!(
        "[voxel] registered base image {}/img/{}@base",
        o.dataset, o.image_name
    );
    Ok(())
}

/// Build a `voxel-frr` router image: stage the prebuilt static agent for the
/// Debian guest, then bake.
pub(crate) async fn create_frr(
    version: &str,
    dataset: &str,
    external: Option<&voxel_config::External>,
) -> Result<()> {
    let network = builder_network(external)?;
    let image_name = format!("voxel-frr-{version}");
    let cargo_bay = fresh_cargo_bay("vbuild-frr")?;
    stage_agent(&ROUTER_INIT, &cargo_bay)?;

    bake(BakeOpts {
        base_image: "debian-13.2",
        role: "frr",
        cargo_bay: &cargo_bay,
        image_name: &image_name,
        dataset,
        disk_gb: 20,
        mem_gb: 16,
        cores: 8,
        network: &network,
    })
    .await?;
    println!("built image {image_name}");
    Ok(())
}

/// Build a `voxel-builder` image: a Helios guest with git, rustup, omicron's
/// builder prerequisites and `omicron_rev` checked out under /work.
pub(crate) async fn create_builder(
    version: &str,
    dataset: &str,
    external: Option<&voxel_config::External>,
    omicron_rev: &str,
) -> Result<()> {
    let network = builder_network(external)?;
    let image_name = format!("voxel-builder-{version}");
    let cargo_bay = fresh_cargo_bay("vbuild-builder")?;
    stage_agent(&SLED_INIT, &cargo_bay)?;
    std::fs::write(cargo_bay.join("omicron-rev"), format!("{omicron_rev}\n"))
        .with_context(|| format!("stage omicron-rev into {cargo_bay}"))?;

    bake(BakeOpts {
        base_image: "helios-3.0",
        role: "builder",
        cargo_bay: &cargo_bay,
        image_name: &image_name,
        dataset,
        disk_gb: 100,
        mem_gb: 16,
        cores: 8,
        network: &network,
    })
    .await?;
    println!("built image {image_name}");
    Ok(())
}

/// An empty `image-build/cargo-bay/<name>`. Root-owned leftovers from a pfexec
/// run would fail every later write.
pub(crate) fn fresh_cargo_bay(name: &str) -> Result<Utf8PathBuf> {
    let cargo_bay = build_root()?.join("cargo-bay").join(name);
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
    Ok(cargo_bay)
}

/// Copy a prebuilt agent into the cargo-bay as `voxel-init`, executable.
pub(crate) fn stage_agent(
    agent: &Prebuilt,
    cargo_bay: &Utf8Path,
) -> Result<()> {
    let src = find_voxel_init(agent)?;
    eprintln!("[voxel] staging voxel-init from {src}");
    let dst = cargo_bay.join("voxel-init");
    std::fs::copy(&src, &dst)
        .with_context(|| format!("stage {src} into the cargo-bay"))?;
    let _ = Command::new("chmod").arg("+x").arg(&dst).status();
    Ok(())
}

/// A prebuilt in-guest agent and where to look for it.
pub(crate) struct Prebuilt {
    /// Which image it goes into, for messages.
    pub role: &'static str,
    /// Environment variable naming the binary outright.
    pub env: &'static str,
    /// Paths relative to the voxel binary, tried in order.
    pub candidates: &'static [&'static str],
    /// The cargo command that produces it from a source checkout.
    pub build: &'static str,
}

/// The native illumos agent for the sled image. A workspace build puts it
/// next to voxel: the baked agent is always the launching binary's era.
pub(crate) const SLED_INIT: Prebuilt = Prebuilt {
    role: "sled image",
    env: "VOXEL_INIT",
    candidates: &["voxel-init"],
    build: "cargo build --release -p voxel-init",
};

/// The static musl agent for the Debian router image. CI publishes it as
/// `voxel-init-linux`; a source checkout cross-compiles it on the host.
pub(crate) const ROUTER_INIT: Prebuilt = Prebuilt {
    role: "router image",
    env: "VOXEL_INIT_LINUX",
    candidates: &[
        "voxel-init-linux",
        "../x86_64-unknown-linux-musl/release/voxel-init",
    ],
    build: "RUSTFLAGS='-C linker=rust-lld -C link-self-contained=yes' \
            cargo build --release -p voxel-init --target x86_64-unknown-linux-musl",
};

/// Locate a prebuilt agent: the environment override, then next to the voxel
/// binary as the CI bundle lays it out, then cargo's own target layout.
pub(crate) fn find_voxel_init(agent: &Prebuilt) -> Result<Utf8PathBuf> {
    if let Ok(p) = std::env::var(agent.env) {
        let p = Utf8PathBuf::from(p);
        if !p.exists() {
            bail!("${}={p} does not exist", agent.env);
        }
        return Ok(p);
    }
    let exe = std::env::current_exe().context("resolve the voxel binary")?;
    let dir = exe.parent().context("the voxel binary has a parent dir")?;
    for rel in agent.candidates {
        let candidate = dir.join(rel);
        if candidate.exists() {
            return Utf8PathBuf::try_from(candidate)
                .context("non-utf8 voxel-init path");
        }
    }
    bail!(
        "no prebuilt voxel-init for the {} next to {}; build it with `{}` \
         or point ${} at one",
        agent.role,
        exe.display(),
        agent.build,
        agent.env
    )
}

/// The host-side image build workspace, `image-build/` under the anchored
/// workdir. Fixed on first use: `bake` cds into it.
pub(crate) fn build_root() -> Result<Utf8PathBuf> {
    static ROOT: OnceLock<Utf8PathBuf> = OnceLock::new();
    if let Some(root) = ROOT.get() {
        return Ok(root.clone());
    }
    let cwd = std::env::current_dir().context("current dir")?;
    let root = Utf8PathBuf::try_from(cwd)
        .context("non-utf8 working directory")?
        .join("image-build");
    std::fs::create_dir_all(&root).with_context(|| format!("mkdir {root}"))?;
    Ok(ROOT.get_or_init(|| root).clone())
}

/// Stage the builder's static address for an isolated-mode build, or clear a
/// stale one. A leftover `builder-net` would override DHCP on a later LAN build.
fn stage_builder_net(
    cargo_bay: &Utf8Path,
    explicit: Option<&str>,
) -> Result<()> {
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

/// Clear the device-instance map (nodes booted from the image otherwise
/// mis-bind vioif instances), then halt cleanly: propolis must flush the zvol.
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

/// Give the halting guest time to flush. Propolis usually outlives the halt;
/// reaching the timeout is the normal path and `hyperstop` follows either way.
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

/// Read a node's propolis pid. Anything but a plain number is rejected: a
/// stray pidfile must not become a `kill` of something unrelated.
fn read_pidfile(name: &str) -> Option<String> {
    let raw = std::fs::read_to_string(falcon_dir().join(format!("{name}.pid")))
        .ok()?;
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
    if let Ok(uuid) = std::fs::read_to_string(dir.join(format!("{name}.uuid")))
    {
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

/// Capture the builder's disk as a registered falcon base image: a full send
/// into a staging dataset, renamed over the target only after it succeeds.
fn capture(dataset: &str, image_name: &str) -> Result<()> {
    let node = Dataset::new(format!("{dataset}/topo/{DEPLOY}/{NODE}"))?;
    let zvol = format!("/dev/zvol/rdsk/{}", node.as_str());
    if !Utf8Path::new(&zvol).exists() {
        bail!("node zvol not found at {zvol}");
    }
    let image = Dataset::new(format!("{dataset}/img/{image_name}"))?;
    // Not `voxel-`-prefixed, so `image ls` never offers a half-streamed image.
    let staging = Dataset::new(format!("{dataset}/img/incoming-{image_name}"))?;
    let replacing = zfs::exists(&image);

    if replacing {
        eprintln!("[voxel] overwriting voxel image {image_name}");
    }
    // Left behind by a capture that died mid-stream.
    zfs::destroy_dataset_if_present(&staging)?;
    // Snapshot of the builder's own disk, which is torn down next.
    zfs::destroy_snapshot_if_present(&node.snapshot("base"))?;

    eprintln!("[voxel] building temp image {}", staging.as_str());
    zfs::snapshot(&node, "base")?;
    zfs::send_recv(&node.snapshot("base"), &staging)?;

    if replacing {
        eprintln!("[voxel] build complete, overwriting image {image_name}");
        zfs::destroy_dataset_if_present(&image)?;
    }
    zfs::rename(&staging, &image)
}
