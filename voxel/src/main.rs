// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The voxel CLI: launch and operate an image backed virtual Oxide rack. This
//! file holds the command tree, config discovery, and the workdir anchoring.

use anyhow::{Context, Error};
use camino::{Utf8Path, Utf8PathBuf};
use clap::{Parser, Subcommand};
use std::fs;
use voxel_config::VoxelConfig;

mod access;
mod commission;
mod commtest;
mod config_cmd;
mod cpbuild;
mod disks;
mod image;
mod imagebuild;
mod isolated_external;
mod net;
mod network;
mod node;
mod patch;
mod power;
mod rack;
mod repocmd;
mod rss;
mod rss_request;
mod sp_cmd;
mod sp_host;
mod topo;
mod tufrepo;
mod util;
mod wicket_setup;

#[derive(Parser)]
#[command(
    name = "voxel",
    version,
    about = "Launch and operate an image-backed virtual Oxide rack"
)]
struct Cli {
    /// voxel.toml to use. Default: ~/.config/voxel/voxel.toml, then /etc/voxel.
    #[arg(long, global = true, env = "VOXEL_CONFIG")]
    config: Option<Utf8PathBuf>,

    /// Project root that cargo-bay/ and .falcon/ live under.
    #[arg(long, global = true, env = "VOXEL_WORKDIR")]
    workdir: Option<Utf8PathBuf>,

    /// Topology name, the falcon deployment.
    #[arg(long, global = true, default_value = "voxel", env = "VOXEL_NAME")]
    name: String,

    /// zfs dataset falcon uses. Default: rpool/falcon.
    #[arg(long, global = true)]
    dataset: Option<String>,

    /// Build root for image create. Default: $HOME/voxel-builds.
    #[arg(long, global = true)]
    build_root: Option<Utf8PathBuf>,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Launch the rack and stream RSS bring-up progress.
    Launch {
        /// Don't watch RSS bring-up after launch.
        #[arg(long)]
        no_progress: bool,
        /// Do not set the host route to the rack's external network.
        #[arg(long)]
        no_route: bool,
        /// Run real firmware SPs and RoTs on sp-emu instead of sp-sim, with
        /// rack setup through wicketd. Firmware comes from the image's repo.
        #[arg(long)]
        emu: bool,
        /// Run the emulated fleet on the firmware in DIR instead of the
        /// image's own, laid out as image create --from-tuf extracts it.
        #[arg(long, value_name = "DIR")]
        sp_firmware: Option<Utf8PathBuf>,
    },
    /// Debug: print the wicketd RSS config body reshaped from a generated
    /// config-rss.toml.
    #[command(hide = true)]
    WicketDryrun {
        /// Path to a generated config-rss.toml.
        config_rss: Utf8PathBuf,
        /// Per-rack sled count, the bootstrap slot set.
        #[arg(default_value_t = 4)]
        sleds: usize,
    },
    /// Debug: print the typed commission rack setup config body as JSON.
    #[command(hide = true)]
    CommissionDryrun {
        /// Rack index, 0-based.
        #[arg(default_value_t = 0)]
        rack: usize,
    },
    /// Point the host route for the rack's external network at ce's current IP.
    Route {
        /// Print the route command instead of applying it.
        #[arg(long)]
        dry_run: bool,
    },
    /// Destroy the rack.
    Destroy,
    /// Open a serial console to a node. ^q exits.
    Serial { node: String },
    /// Print topology information.
    Info,
    /// Watch RSS bring-up progress on a running rack.
    Status,
    /// Inspect or edit configuration.
    Config {
        #[command(subcommand)]
        cmd: ConfigCmd,
    },
    /// Manage image bundles.
    Image {
        #[command(subcommand)]
        cmd: ImageCmd,
    },
    /// Operate on a running rack: component patching.
    Rack {
        #[command(subcommand)]
        cmd: RackCmd,
    },
    /// Inspect, configure, and validate the rack network.
    Network {
        #[command(subcommand)]
        cmd: NetworkCmd,
    },
    /// Operate and manage the emulated SPs of an --emu rack.
    Sp {
        #[command(subcommand)]
        cmd: SpCmd,
    },
    /// Access a sled's global zone: ls, login, exec.
    Host {
        #[command(subcommand)]
        cmd: HostCmd,
    },
    /// Access a switch zone, the technician port: ls, login, exec.
    Tp {
        #[command(subcommand)]
        cmd: TpCmd,
    },
    /// TUF repo operator helpers against a live rack.
    Repo {
        #[command(subcommand)]
        cmd: RepoCmd,
    },
    /// Build and run omicron's commit matched connectivity test. Arguments
    /// after -- go to commtest; voxel supplies the rack API and an IP pool.
    Commtest {
        /// Omicron commit or tag to test, or main. Default: the commit of the
        /// configured control plane image.
        #[arg(value_name = "COMMIT")]
        reference: Option<String>,

        /// Use an existing Omicron checkout without fetching or changing it.
        #[arg(long, value_name = "PATH", conflicts_with = "reference")]
        source: Option<Utf8PathBuf>,

        /// Rack to target, 1-based.
        #[arg(long, default_value_t = 1)]
        rack: usize,

        /// Override the derived Nexus API URL.
        #[arg(long, value_name = "URL")]
        api: Option<String>,

        /// Connectivity phase to run. uni and multi are accepted aliases.
        #[arg(long, value_enum, default_value_t = commtest::Traffic::Unicast)]
        traffic: commtest::Traffic,

        /// Run an already-built commtest binary.
        #[arg(long)]
        no_build: bool,

        /// Permit running as root. Artifacts under the build root then become
        /// root owned.
        #[arg(long)]
        allow_root: bool,

        /// Arguments passed to commtest, after --.
        #[arg(last = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
}

#[derive(Subcommand)]
enum ConfigCmd {
    /// Print the effective configuration.
    Show,
    /// Read a dotted key such as network.bgp_asn.
    Get { key: String },
    /// Set a dotted scalar key such as topology.sleds 3.
    Set { key: String, value: String },
    /// Validate and install a prepared voxel.toml.
    Load { file: Utf8PathBuf },
}

#[derive(Subcommand)]
enum ImageCmd {
    /// List image bundles on disk.
    #[command(visible_alias = "list")]
    Ls,
    /// Build a voxel-cp image for an omicron commit from source, or from an
    /// existing checkout as is with --src.
    Create {
        /// omicron commit or tag to build and pin the image to. Default: the
        /// rev voxel pins, or the repo's with --from-tuf. A label with --src.
        commit: Option<String>,
        /// Build an existing omicron checkout as is, skipping clone and
        /// checkout.
        #[arg(long)]
        src: Option<Utf8PathBuf>,
        /// Build the image from this TUF repo's artifacts with no omicron
        /// compile. The switch zone is recomposed for softnpu.
        #[arg(long, value_name = "REPO_ZIP")]
        from_tuf: Option<Utf8PathBuf>,
        /// With --from-tuf: an omicron-sled-agent package tar staged in place
        /// of the phase 2 sled-agent, for trying a sled-agent build. The
        /// phase 2 one detects softnpu scrimlets at runtime.
        #[arg(long, value_name = "PKG_TAR", requires = "from_tuf")]
        sled_agent: Option<Utf8PathBuf>,
    },
    /// Export an image bundle: a zstd compressed zfs stream, or a raw xz disk
    /// image with --raw.
    Export {
        /// Image name.
        name: String,
        /// Output file. Default: <name>.zfs.zst, or <name>.raw.xz with --raw.
        out: Option<Utf8PathBuf>,
        /// Portable raw disk image instead of a zfs stream.
        #[arg(long)]
        raw: bool,
    },
    /// Import an image bundle from image export, .zfs.zst or .raw.xz.
    Import {
        /// File to import. The image name derives from it.
        file: Utf8PathBuf,
    },
    /// Remove an image bundle.
    Rm {
        /// Image name to remove.
        name: String,
        /// Don't prompt for confirmation.
        #[arg(long)]
        yes: bool,
    },
    /// Fold a component patch into the image so it survives relaunches: boot,
    /// place the artifact, recapture. propolis and ddm-gz only for now.
    Patch {
        /// Component to patch, propolis or ddm-gz.
        component: String,
        /// Git commit to patch to.
        reference: String,
        /// Source image to patch. Default: the configured image.cp.
        #[arg(long)]
        image: Option<String>,
        /// New image name. Default: <src>-<component>-<shortref>.
        #[arg(long)]
        out: Option<String>,
    },
    /// Build a voxel-frr customer router image.
    CreateFrr {
        /// Image label. The image is named voxel-frr-<version>.
        #[arg(default_value = "proto")]
        version: String,
    },
    /// Build helper: boot a one node builder, run the agent's install role,
    /// capture the disk.
    #[command(hide = true)]
    Bake {
        /// Registered image name, captured to <dataset>/img/<name>@base.
        name: String,
        /// Base image the builder boots.
        #[arg(long, default_value = "helios-3.0")]
        base: String,
        /// Agent install role, cp or frr.
        #[arg(long)]
        role: Option<String>,
        /// An in-guest command to run instead of an agent role. With neither,
        /// the builder only boots.
        #[arg(long, conflicts_with = "role")]
        exec: Option<String>,
        /// Host dir mounted at /opt/cargo-bay in the guest.
        #[arg(long, default_value = "./cargo-bay/vbuild")]
        cargo_bay: Utf8PathBuf,
        #[arg(long, default_value_t = 8)]
        cores: u8,
        #[arg(long, default_value_t = 16)]
        mem_gb: u64,
        #[arg(long, default_value_t = 100)]
        disk_gb: u64,
        /// falcon deployment name for the builder topology.
        #[arg(long, default_value = "voxel_build")]
        deploy: String,
        /// Host link the builder reaches the package repos through. Default:
        /// falcon's external interface.
        #[arg(long)]
        ext_interface: Option<String>,
    },
    /// Build helper: render the build time smf configs into an omicron
    /// checkout.
    #[command(hide = true)]
    RenderSmf {
        /// Path to the omicron checkout root.
        omicron_root: Utf8PathBuf,
        /// Number of gimlet SPs to simulate.
        #[arg(long, default_value_t = 4)]
        gimlets: usize,
    },
}

#[derive(Subcommand)]
enum NetworkCmd {
    /// Show the per-rack network projection, switches, and the cross-rack
    /// interconnect mesh.
    Show,
    /// Bring up a switch port's link on a running rack. Transient: Nexus reaps
    /// manual swadm links; persistent config goes through the API.
    LinkUp {
        /// Switch: switchN, rackR/switchS, or a scrimlet node gN.
        switch: String,
        /// Switch port such as qsfp2. The link is created as <port>/0.
        port: String,
        /// Link speed. Default 40G, matching the qsfp uplinks.
        #[arg(long, default_value = "40G")]
        speed: String,
        /// Forward error correction. Default none.
        #[arg(long, default_value = "none")]
        fec: String,
    },
    /// Take down a switch port's link on a running rack: disable and delete.
    LinkDown { switch: String, port: String },
    /// Validate live networking: links, BGP sessions, routes, host routes.
    Validate {
        /// Full swadm and mgadm output instead of summary counts.
        #[arg(long)]
        detail: bool,
    },
    /// Manage the isolated external segment, [external] mode = isolated.
    External {
        #[command(subcommand)]
        cmd: ExternalCmd,
    },
}

#[derive(Subcommand)]
enum ExternalCmd {
    /// Stand the segment up, as launch does.
    Up {
        /// Print the host commands instead of running them.
        #[arg(long)]
        dry_run: bool,
    },
    /// Tear the segment down: VNIC, etherstub, NAT. ipv4-forwarding stays.
    Down {
        /// Print the host commands instead of running them.
        #[arg(long)]
        dry_run: bool,
    },
    /// Assert the whole path is live: uplink, links, NAT. PASS or FAIL each.
    Check,
}

#[derive(Subcommand)]
enum RackCmd {
    /// Swap one component on the running rack at a ref, from buildomat, and
    /// restart it. Ephemeral: a relaunch reverts. --list shows the components.
    Patch {
        /// Component to patch, such as propolis or mgd. Omit with --list.
        component: Option<String>,
        /// Git commit to patch to, the buildomat image revision.
        reference: Option<String>,
        /// List the patchable components and exit.
        #[arg(long)]
        list: bool,
        /// Print the plan without applying it.
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Subcommand)]
enum SpCmd {
    /// List the live SPs over MGS: type, serial, power, archive id. Needs a
    /// running --emu rack.
    #[command(visible_alias = "list")]
    Ls {
        /// Which switch view to query: switch0, switch1, or a scrimlet.
        #[arg(long, default_value = "switch0")]
        switch: String,
    },
    /// Show one SP's state: serial, power, RoT, archive.
    #[command(visible_alias = "state")]
    Info {
        /// Target SP: a serial, a node such as sidecar or g0, or a port.
        target: String,
        #[arg(long, default_value = "switch0")]
        switch: String,
    },
    /// Show an SP's power state.
    #[command(visible_alias = "st")]
    Status {
        /// Target SP: a serial, a node such as sidecar or g0, or a port.
        target: String,
        #[arg(long, default_value = "switch0")]
        switch: String,
    },
    /// Inject an NMI into the host via the SP.
    Nmi {
        /// Target SP: a serial, a node such as sidecar or g0, or a port.
        target: String,
        #[arg(long, default_value = "switch0")]
        switch: String,
    },
    /// Power cycle a host through its SP, as pilot sp cycle does: A2 if it is
    /// on, then A0. The sled VM follows the SP.
    Cycle {
        /// Target SP: a serial, a node such as g0, or a port.
        target: String,
        #[arg(long, default_value = "switch0")]
        switch: String,
    },
    /// Follow the emulated SPs and keep the sleds in step with their host
    /// power. launch --emu runs this as svc:/oxide/voxel-power.
    #[command(hide = true)]
    Watch {
        /// Rack to follow, 0-based. Default: every rack.
        #[arg(long)]
        rack: Option<usize>,
    },
    /// Pass a raw faux-mgs command to an SP. Everything after -e is passed
    /// through, quoted or not.
    Exec {
        /// Target SP: a serial, a node such as sidecar or g0, or a port.
        target: String,
        #[arg(long, default_value = "switch0")]
        switch: String,
        /// The faux-mgs command and its arguments.
        #[arg(short = 'e', long = "exec", num_args = 1.., allow_hyphen_values = true)]
        command: Vec<String>,
    },
    /// Check whether launch --emu has its artifacts. No rack needed.
    Ready,
    /// Flash a hubris archive into an sp-emu slot A flash file, offline.
    Flash {
        /// Hubris image archive.
        image: Utf8PathBuf,
        /// Output flash file.
        out: Utf8PathBuf,
    },
    /// Re-flash a live SP, or the shared RoT with target rot, and restart its
    /// service. Ephemeral: a relaunch reverts.
    Reflash {
        /// Target: sidecar, gN, a port, or rot.
        target: String,
        /// Hubris archive for an SP, or a raw oxide-rot-1 image for rot.
        image: Utf8PathBuf,
        #[arg(long, default_value = "switch0")]
        switch: String,
    },
    /// Enable, or with --off disable, the humility SWD debug listeners for one
    /// SP. Restarts the SP. Ephemeral.
    Debug {
        /// Target SP: sidecar, gN, or a port.
        target: String,
        /// Disable debug instead of enabling it.
        #[arg(long)]
        off: bool,
        #[arg(long, default_value = "switch0")]
        switch: String,
    },
    /// Force and decode a crash dump of one emulated SP with humility hydrate.
    /// Needs humility on PATH or in $VOXEL_HUMILITY.
    Dump {
        /// Target SP: sidecar, gN, or a port.
        target: String,
        /// Run humility ringbuf instead of tasks on the hydrated dump.
        #[arg(long)]
        ringbuf: bool,
        #[arg(long, default_value = "switch0")]
        switch: String,
    },
    /// Drive one host to SP IPCC exchange over the SP's control UART (RFD 316)
    /// and decode the reply.
    Ipcc {
        /// Target SP: sidecar, gN, or a port.
        target: String,
        /// Request to send: identity, bsu, macs, status, or inventory.
        #[arg(long, default_value = "identity")]
        cmd: String,
        #[arg(long, default_value = "switch0")]
        switch: String,
    },
    /// Build the gimlet-c and sidecar-c-emu images from a hubris commit.
    Build {
        /// hubris commit to build. v1 builds from the configured checkout.
        commit: String,
    },
}

#[derive(Subcommand)]
enum HostCmd {
    /// List nodes: external IP, role, propolis pid and state, SP host power.
    Ls,
    /// SSH into a sled's global zone or a router.
    Login {
        #[arg(default_value = "g0")]
        node: String,
    },
    /// Run a command in a sled's global zone.
    Exec {
        /// Command to run. Quote multi-word commands.
        #[arg(short = 'c', long = "command")]
        command: String,
        /// Target sled, such as g1.
        #[arg(default_value = "g0")]
        sled: String,
    },
    /// Power a node off: stop its propolis and destroy the VM. An emulated
    /// SP is told its host went away and reports A2.
    Off { node: String },
    /// Power a node on: a fresh propolis replaying the instance it launched
    /// with. With an emulated SP, sp cycle is the SP's way to do it.
    On { node: String },
    /// Reset a node: a propolis reboot. The VM keeps its process.
    Reset { node: String },
}

#[derive(Subcommand)]
enum TpCmd {
    /// List switch zones and their external IPs.
    Ls,
    /// SSH into a switch zone, where swadm, dpd and mgadm live.
    Login {
        #[arg(default_value = "switch0")]
        switch: String,
    },
    /// Run a command in a switch zone.
    Exec {
        /// Command to run in oxz_switch. Quote multi-word commands.
        #[arg(short = 'c', long = "command")]
        command: String,
        /// Target switch: switchN, rackR/switchS, or a scrimlet node.
        #[arg(default_value = "switch0")]
        switch: String,
    },
}

#[derive(Subcommand)]
enum RepoCmd {
    /// Seed every sled's artifact store with a repo's targets so a new target
    /// release converges without waiting out TUF replication.
    Seed {
        /// The TUF repo zip that was uploaded.
        repo: Utf8PathBuf,
    },
}

// Config loading and project root resolution.

fn config_text(path: &Utf8Path) -> anyhow::Result<String> {
    if path.exists() {
        Ok(fs::read_to_string(path)
            .with_context(|| format!("read {}", path))?)
    } else {
        Ok(VoxelConfig::default().to_toml())
    }
}

fn load_config(path: &Utf8Path) -> anyhow::Result<VoxelConfig> {
    let text = config_text(path)?;
    let cfg = VoxelConfig::from_toml(&text)
        .with_context(|| format!("parse {}", path))?;
    cfg.topology.validate().map_err(|e| anyhow::anyhow!("{path}: {e}"))?;
    Ok(cfg)
}

/// Make a path absolute against the current directory.
fn absolutize(p: Utf8PathBuf) -> Utf8PathBuf {
    if p.is_absolute() {
        p
    } else {
        let cwd = std::env::current_dir()
            .ok()
            .and_then(|d| Utf8PathBuf::try_from(d).ok())
            .unwrap_or_default();
        cwd.join(p)
    }
}

/// The voxel.toml to use, absolute: --config, then ~/.config/voxel/voxel.toml,
/// then /etc/voxel/voxel.toml. Falls back to ./voxel.toml without $HOME.
fn discover_config(explicit: Option<&Utf8Path>) -> Utf8PathBuf {
    if let Some(p) = explicit {
        return absolutize(p.to_path_buf());
    }
    let user = std::env::var("HOME")
        .ok()
        .map(|h| Utf8PathBuf::from(h).join(".config/voxel/voxel.toml"));
    if let Some(cand) = &user
        && cand.is_file()
    {
        return cand.clone();
    }
    let etc = Utf8PathBuf::from("/etc/voxel/voxel.toml");
    if etc.is_file() {
        return etc;
    }
    // Nothing exists yet: default to the user config so config set creates
    // it there.
    user.unwrap_or_else(|| absolutize(Utf8PathBuf::from("voxel.toml")))
}

/// Locate <build_root>/omicron-<commit>, matching a short label against full
/// sha checkout dirs and vice versa. Ambiguous matches use the exact path.
fn find_omicron_checkout(build_root: &str, commit: &str) -> String {
    let exact = format!("{build_root}/omicron-{commit}");
    if Utf8PathBuf::from(&exact).is_dir() {
        return exact;
    }
    let mut candidates: Vec<String> = std::fs::read_dir(build_root)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter_map(|n| {
            let sha = n.strip_prefix("omicron-")?;
            (sha.starts_with(commit) || commit.starts_with(sha))
                .then(|| format!("{build_root}/{n}"))
        })
        .collect();
    if candidates.len() == 1 { candidates.pop().unwrap() } else { exact }
}

/// Resolve the falcon settings, flag over config over environment, and export
/// them as FALCON_DATASET and VOXEL_OMICRON_SRC for every subprocess.
fn resolve_falcon_env(cli: &Cli, cfg: Option<&VoxelConfig>) {
    let dataset = cli
        .dataset
        .clone()
        .or_else(|| cfg.and_then(|c| c.falcon.dataset.clone()))
        .or_else(|| std::env::var("FALCON_DATASET").ok());
    if let Some(d) = dataset {
        // SAFETY: runs before the tokio runtime spawns any worker, so no
        // concurrent getenv can race the write.
        unsafe {
            std::env::set_var("FALCON_DATASET", d);
        }
    }
    // Resolve the build root first; the omicron source path derives from it.
    // Export it as is and apply the default only to the derivation.
    let build_root = cli
        .build_root
        .as_ref()
        .map(|p| p.to_string())
        .or_else(|| cfg.and_then(|c| c.falcon.build_root.clone()))
        .or_else(|| std::env::var("BUILD_ROOT").ok());
    if let Some(b) = &build_root {
        // SAFETY: same single-threaded argument as FALCON_DATASET above.
        unsafe {
            std::env::set_var("BUILD_ROOT", b);
        }
    }
    let build_root_eff = build_root.unwrap_or_else(|| {
        format!(
            "{}/voxel-builds",
            std::env::var("HOME").unwrap_or_else(|_| "/root".into())
        )
    });
    // The omicron checkout the CP image was built from, derived from the
    // image's commit. $VOXEL_OMICRON_SRC overrides.
    let omicron_src = std::env::var("VOXEL_OMICRON_SRC").ok().or_else(|| {
        cfg.and_then(|c| c.image.cp_commit())
            .map(|commit| find_omicron_checkout(&build_root_eff, &commit))
    });
    if let Some(s) = omicron_src {
        // SAFETY: same single-threaded argument as FALCON_DATASET above.
        unsafe {
            std::env::set_var("VOXEL_OMICRON_SRC", s);
        }
    }
}

/// chdir to the project root so cargo-bay and .falcon resolve: --workdir,
/// then [falcon].workdir, then the config's directory. No-op if not a dir.
fn anchor_workdir(
    cli: &Cli,
    cfg: Option<&VoxelConfig>,
    config_path: &Utf8Path,
) -> anyhow::Result<()> {
    let root = cli
        .workdir
        .clone()
        .or_else(|| {
            cfg.and_then(|c| c.falcon.workdir.clone()).map(Utf8PathBuf::from)
        })
        .or_else(|| config_path.parent().map(Utf8Path::to_path_buf));
    if let Some(root) = root
        && root.is_dir()
    {
        std::env::set_current_dir(&root)
            .with_context(|| format!("chdir to workdir {}", root))?;
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    let cli = Cli::parse();
    // Anchor to the project root before anything touches cargo-bay or .falcon.
    let config_path = discover_config(cli.config.as_deref());
    // A missing config means defaults. An existing one that fails to parse
    // is an error.
    let cfg = match load_config(&config_path) {
        Ok(c) => Some(c),
        Err(e) if config_path.is_file() => return Err(e),
        Err(_) => None,
    };
    resolve_falcon_env(&cli, cfg.as_ref());
    anchor_workdir(&cli, cfg.as_ref(), &config_path)?;
    match &cli.cmd {
        Cmd::Launch { no_progress, no_route, emu, sp_firmware } => {
            // One flag: emulated SPs, the RoT bridge and wicketd setup are one
            // configuration in practice.
            rack::cmd_launch(
                &load_config(&config_path)?,
                &cli.name,
                &config_path,
                *no_progress,
                *no_route,
                *emu,
                sp_firmware.as_deref(),
            )
            .await
        }
        Cmd::WicketDryrun { config_rss, sleds } => {
            wicket_setup::dryrun(config_rss, *sleds)
        }
        Cmd::CommissionDryrun { rack } => {
            commission::dryrun(&load_config(&config_path)?, *rack)
        }
        Cmd::Route { dry_run } => {
            rack::cmd_route(&load_config(&config_path)?, &cli.name, *dry_run)
                .await
        }
        Cmd::Destroy => {
            rack::cmd_destroy(&load_config(&config_path)?, &cli.name)
        }
        Cmd::Serial { node } => {
            access::cmd_serial(&load_config(&config_path)?, &cli.name, node)
                .await
        }
        Cmd::Info => rack::cmd_info(&load_config(&config_path)?, &cli.name),
        Cmd::Status => {
            rack::cmd_status(&load_config(&config_path)?, &cli.name).await
        }
        Cmd::Commtest {
            reference,
            source,
            rack,
            api,
            traffic,
            no_build,
            allow_root,
            args,
        } => commtest::run(
            &load_config(&config_path)?,
            commtest::Options {
                // clap rejects COMMIT together with --source.
                source: match (source.as_deref(), reference.as_deref()) {
                    (Some(path), _) => commtest::Source::Local(path),
                    (None, Some(r)) => commtest::Source::Reference(r),
                    (None, None) => commtest::Source::Image,
                },
                rack: *rack,
                api_override: api.as_deref(),
                traffic: *traffic,
                no_build: *no_build,
                allow_root: *allow_root,
                passthrough: args,
            },
        ),
        Cmd::Config { cmd } => config_cmd::cmd_config(&config_path, cmd),
        Cmd::Image { cmd } => match cmd {
            ImageCmd::Patch { component, reference, image, out } => {
                let cfg = load_config(&config_path)?;
                let src = image.clone().unwrap_or_else(|| cfg.image.cp_image());
                patch::cmd_image_patch(
                    component,
                    reference,
                    &src,
                    out.as_deref(),
                )
            }
            ImageCmd::Create { commit, src, from_tuf, sled_agent } => {
                cpbuild::create(
                    commit.as_deref(),
                    src.as_deref(),
                    from_tuf.as_deref(),
                    sled_agent.as_deref(),
                    &image::falcon_dataset(),
                    cfg.as_ref().map(|c| &c.external),
                )
                .await
            }
            ImageCmd::CreateFrr { version } => {
                imagebuild::create_frr(
                    version,
                    &image::falcon_dataset(),
                    cfg.as_ref().map(|c| &c.external),
                )
                .await
            }
            ImageCmd::Bake {
                name,
                base,
                role,
                exec,
                cargo_bay,
                cores,
                mem_gb,
                disk_gb,
                deploy,
                ext_interface,
            } => {
                let dataset = image::falcon_dataset();
                imagebuild::bake(imagebuild::BakeOpts {
                    base_image: base,
                    role: role.as_deref(),
                    exec: exec.as_deref(),
                    cargo_bay,
                    image_name: name,
                    dataset: &dataset,
                    deploy,
                    disk_gb: *disk_gb,
                    mem_gb: *mem_gb,
                    cores: *cores,
                    network: &imagebuild::BuilderNetwork {
                        interface: ext_interface.clone(),
                        static_address: None,
                    },
                })
                .await
            }
            other => image::cmd_image(
                other,
                cfg.as_ref().map(|c| c.image.cp_image()),
            ),
        },
        Cmd::Network { cmd } => match cmd {
            NetworkCmd::Show => network::show(&load_config(&config_path)?),
            NetworkCmd::LinkUp { switch, port, speed, fec } => {
                network::link_up(
                    &load_config(&config_path)?,
                    &cli.name,
                    switch,
                    port,
                    speed,
                    fec,
                )
                .await
            }
            NetworkCmd::LinkDown { switch, port } => {
                network::link_down(
                    &load_config(&config_path)?,
                    &cli.name,
                    switch,
                    port,
                )
                .await
            }
            NetworkCmd::Validate { detail } => {
                network::validate(
                    &load_config(&config_path)?,
                    &cli.name,
                    *detail,
                )
                .await
            }
            NetworkCmd::External { cmd } => {
                let cfg = load_config(&config_path)?;
                match cmd {
                    ExternalCmd::Up { dry_run } => isolated_external::up(
                        &cfg.external,
                        isolated_external::DryRun::from_flag(*dry_run),
                    ),
                    ExternalCmd::Down { dry_run } => isolated_external::down(
                        &cfg.external,
                        isolated_external::DryRun::from_flag(*dry_run),
                    ),
                    ExternalCmd::Check => {
                        isolated_external::check(&cfg.external)
                    }
                }
            }
        },
        Cmd::Rack { cmd } => match cmd {
            RackCmd::Patch { component, reference, list, dry_run } => {
                if *list {
                    patch::list();
                    Ok(())
                } else {
                    let component = component.as_deref().context(
                        "missing component (try `voxel rack patch --list`)",
                    )?;
                    let reference = reference.as_deref().with_context(|| {
                        format!("missing ref (usage: voxel rack patch {component} <ref>)")
                    })?;
                    patch::cmd_rack_patch(
                        &load_config(&config_path)?,
                        &cli.name,
                        component,
                        reference,
                        *dry_run,
                    )
                    .await
                }
            }
        },
        Cmd::Sp { cmd } => {
            sp_cmd::cmd_sp(&load_config(&config_path)?, &cli.name, cmd).await
        }
        Cmd::Host { cmd } => match cmd {
            HostCmd::Ls => {
                access::cmd_host_ls(&load_config(&config_path)?, &cli.name)
                    .await
            }
            HostCmd::Login { node } => {
                access::cmd_host_login(
                    &load_config(&config_path)?,
                    &cli.name,
                    node,
                )
                .await
            }
            HostCmd::Exec { command, sled } => {
                access::cmd_host_exec(
                    &load_config(&config_path)?,
                    &cli.name,
                    sled,
                    command,
                )
                .await
            }
            HostCmd::Off { node } => {
                node::cmd_host_power(
                    &load_config(&config_path)?,
                    &cli.name,
                    node,
                    node::Power::Off,
                )
                .await
            }
            HostCmd::On { node } => {
                node::cmd_host_power(
                    &load_config(&config_path)?,
                    &cli.name,
                    node,
                    node::Power::On,
                )
                .await
            }
            HostCmd::Reset { node } => {
                node::cmd_host_power(
                    &load_config(&config_path)?,
                    &cli.name,
                    node,
                    node::Power::Reset,
                )
                .await
            }
        },
        Cmd::Tp { cmd } => match cmd {
            TpCmd::Ls => {
                access::cmd_tp_ls(&load_config(&config_path)?, &cli.name).await
            }
            TpCmd::Login { switch } => {
                access::cmd_tp_login(
                    &load_config(&config_path)?,
                    &cli.name,
                    switch,
                )
                .await
            }
            TpCmd::Exec { command, switch } => {
                access::cmd_tp_exec(
                    &load_config(&config_path)?,
                    &cli.name,
                    switch,
                    command,
                )
                .await
            }
        },
        Cmd::Repo { cmd } => match cmd {
            RepoCmd::Seed { repo } => {
                repocmd::cmd_repo_seed(
                    &load_config(&config_path)?,
                    &cli.name,
                    repo,
                )
                .await
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::find_omicron_checkout;

    #[test]
    fn checkout_lookup_matches_short_and_full_shas() {
        let root = crate::util::temp_dir().join("voxel-checkout-lookup");
        let _ = std::fs::remove_dir_all(&root);
        for d in ["omicron-21dae8a64f00baa5", "omicron-43bb5af", "rss-gen-x"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        let find = |c: &str| find_omicron_checkout(root.as_str(), c);
        // Exact directory wins untouched.
        assert!(find("43bb5af").ends_with("omicron-43bb5af"));
        // Short image label resolves to the full-sha checkout dir.
        assert!(find("21dae8a64").ends_with("omicron-21dae8a64f00baa5"));
        // Full sha resolves to a short-named checkout dir.
        assert!(find("43bb5af99ec").ends_with("omicron-43bb5af"));
        // No match falls back to the exact, nonexistent path.
        assert!(find("deadbeef").ends_with("omicron-deadbeef"));
    }
}
