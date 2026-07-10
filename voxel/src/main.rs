//! Voxel - a first-class CLI for launching and operating an image-backed
//! virtual Oxide rack.
//!
//! Sleds boot the `voxel-cp` image, routers boot `voxel-frr`, and every
//! per-node config is generated on the fly from a [`VoxelConfig`] (`voxel.toml`)
//! by the `voxel-config` crate - no static a4x2 files. libfalcon is used as a
//! library (`Runner`), so `voxel` owns its own command tree rather than wrapping
//! falcon's CLI.
//!
//! libfalcon -> Helios only. Run after building voxel-cp / voxel-frr images.
//!
//! This file holds the CLI surface (clap), config discovery/loading, and the
//! startup that anchors voxel to its project root; the commands themselves live
//! in the topic modules below.

use anyhow::{anyhow, Context, Error};
use clap::{Parser, Subcommand};
use std::fs;
use std::path::{Path, PathBuf};
use voxel_config::VoxelConfig;

mod access;
mod config_cmd;
mod image;
mod net;
mod network;
mod patch;
mod rack;
mod rss;
mod sp_cmd;
mod topo;
mod util;
mod wicket_setup;

#[derive(Parser)]
#[command(
    name = "voxel",
    version,
    about = "Launch and operate an image-backed virtual Oxide rack"
)]
struct Cli {
    /// voxel.toml to use (default: ~/.config/voxel/voxel.toml, then /etc/voxel/voxel.toml).
    #[arg(long, global = true, env = "VOXEL_CONFIG")]
    config: Option<PathBuf>,

    /// Project root that cargo-bay/ and .falcon/ live under.
    #[arg(long, global = true, env = "VOXEL_WORKDIR")]
    workdir: Option<PathBuf>,

    /// Topology (falcon deployment) name.
    #[arg(long, global = true, default_value = "voxel", env = "VOXEL_NAME")]
    name: String,

    /// zfs dataset falcon uses (default: `rpool/falcon`).
    #[arg(long, global = true)]
    dataset: Option<String>,

    /// Override the `voxel-rss-gen` path (default: derived from the image's commit).
    #[arg(long, global = true)]
    rss_gen: Option<PathBuf>,

    /// Build root for `image create` (default: `$HOME/voxel-builds`).
    #[arg(long, global = true)]
    build_root: Option<PathBuf>,

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
        /// Don't set the host route to the rack's external network after launch.
        #[arg(long)]
        no_route: bool,
        /// Run real-firmware SPs on `sp-emu` instead of `sp-sim`.
        ///
        /// The whole fleet. Needs `[sp].emu_bin` + the hubris images in `[sp]`.
        #[arg(long)]
        emu_sp: bool,
        /// Also wire the RoT bridge (oxide-rot-1) onto the sidecar SP. Implies --emu-sp.
        ///
        /// Needs `[sp].rot_image`. Runs the sidecar as two emulated cores; keep
        /// OFF during initial bring-up (it wedges handoff).
        #[arg(long = "emu-rot")]
        emu_rot: bool,
        /// Drive rack setup through wicketd (the real operator flow).
        ///
        /// Suppresses the staged config-rss so sled-agent waits, then uploads the
        /// config + a self-signed cert + recovery password to wicketd and POSTs to
        /// start RSS - fully populating wicket's RACK SETUP page.
        #[arg(long = "wicket-setup")]
        wicket_setup: bool,
    },
    /// (debug) Print the wicketd RSS config body that `--wicket-setup` would PUT,
    /// reshaped from a generated config-rss.toml (validates the mapping offline).
    #[command(hide = true)]
    WicketDryrun {
        /// Path to a generated config-rss.toml.
        config_rss: PathBuf,
        /// Per-rack sled count (the bootstrap slot set).
        #[arg(default_value_t = 4)]
        sleds: usize,
    },
    /// (Re)point the host route for the rack's external net at ce's current IP.
    Route {
        /// Print the route command instead of applying it.
        #[arg(long)]
        dry_run: bool,
    },
    /// Destroy the rack.
    Destroy,
    /// Open a serial console to a node (^q to exit).
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
    /// Operate on a running rack (surgical component patching).
    Rack {
        #[command(subcommand)]
        cmd: RackCmd,
    },
    /// Inspect, configure, and validate the rack network.
    Network {
        #[command(subcommand)]
        cmd: NetworkCmd,
    },
    /// Operate and manage the emulated SPs (`sp-emu`) of an `--emu` rack.
    Sp {
        #[command(subcommand)]
        cmd: SpCmd,
    },
    /// Access a sled's global zone (ls / login / exec).
    Host {
        #[command(subcommand)]
        cmd: HostCmd,
    },
    /// Access a switch zone / technician port (ls / login / exec).
    Tp {
        #[command(subcommand)]
        cmd: TpCmd,
    },
}

#[derive(Subcommand)]
enum ConfigCmd {
    /// Print the effective configuration.
    Show,
    /// Read a dotted key, e.g. `network.bgp_asn`.
    Get { key: String },
    /// Set a dotted scalar key, e.g. `topology.sleds 3`.
    Set { key: String, value: String },
    /// Validate and install a prepared voxel.toml.
    Load { file: PathBuf },
}

#[derive(Subcommand)]
enum ImageCmd {
    /// List image bundles on disk.
    #[command(visible_alias = "list")]
    Ls,
    /// Build a `voxel-cp` image for an omicron commit (from source).
    Create {
        /// omicron git commit (or tag) to build and pin the image to.
        commit: String,
    },
    /// Export an image bundle to a file for distribution.
    ///
    /// Default: a `zfs send | zstd` stream (`<name>.zfs.zst`); `--raw` makes a
    /// portable `<name>.raw.xz` disk image instead.
    Export {
        /// Image name (e.g. `voxel-cp-a3fee0ec`).
        name: String,
        /// Output file (default `<name>.zfs.zst`, or `<name>.raw.xz` with --raw).
        out: Option<PathBuf>,
        /// Portable raw disk image (`dd | xz`) instead of a zfs stream.
        #[arg(long)]
        raw: bool,
    },
    /// Import an image bundle (`.zfs.zst` or `.raw.xz`) from `image export`.
    Import {
        /// File to import (name is derived from it).
        file: PathBuf,
    },
    /// Remove an image bundle (`zfs destroy <dataset>/img/<name>`).
    Rm {
        /// Image name to remove.
        name: String,
        /// Don't prompt for confirmation.
        #[arg(long)]
        yes: bool,
    },
    /// Fold a component patch into the image's @base so it survives relaunches.
    ///
    /// Boot-modify-capture: boots the source image, places the artifact in-guest,
    /// re-captures. The durable counterpart to `rack patch` (slower, ~minutes).
    /// propolis + ddm-gz only for now (switch-zone services need a repack).
    Patch {
        /// Component to patch (e.g. propolis, ddm-gz).
        component: String,
        /// Git ref (commit) to patch to.
        reference: String,
        /// Source image to patch (default: the configured image.cp).
        #[arg(long)]
        image: Option<String>,
        /// New image name (default: <src>-<component>-<shortref>).
        #[arg(long)]
        out: Option<String>,
    },
    /// (build helper) Render the build-time smf configs (mgs-sim, sp-sim,
    /// sled-agent) into an omicron checkout. Used by build-cp.sh.
    #[command(hide = true)]
    RenderSmf {
        /// Path to the omicron checkout root.
        omicron_root: PathBuf,
        /// Number of gimlet SPs to simulate (sp-sim).
        #[arg(long, default_value_t = 4)]
        gimlets: usize,
    },
}

#[derive(Subcommand)]
enum NetworkCmd {
    /// Show the per-rack network projection, switches, and switch interconnects.
    Show,
    /// Add a switch-to-switch interconnect; applied on the next launch.
    ///
    /// A direct sidecar<->sidecar QSFP link carrying the underlay. Selectors:
    /// `switch0` | `switch1` | `switchN` | `rackR/switchS`.
    AddPort { a: String, b: String },
    /// Remove a switch interconnect (either order); applied on the next launch.
    RmPort { a: String, b: String },
    /// Bring up a switch port's link on a running rack (transient).
    ///
    /// Creates (if needed) + enables the link via `swadm`; run on both ends. ⚠
    /// Nexus reaps manual swadm links in ~30s - persistent config must use the API.
    LinkUp {
        /// Switch: `switch0` | `switch1` | `switchN` | `rackR/switchS` | node `gN`.
        switch: String,
        /// Switch port (e.g. `qsfp2`); the link is created as `<port>/0`.
        port: String,
        /// Link speed (default 40G, matching the qsfp uplinks).
        #[arg(long, default_value = "40G")]
        speed: String,
        /// Forward error correction (default none).
        #[arg(long, default_value = "none")]
        fec: String,
    },
    /// Take down a switch port's link (disable + delete) on a running rack,
    /// e.g. `voxel network link-down switch0 qsfp2`.
    LinkDown {
        switch: String,
        port: String,
    },
    /// Validate live networking: link states, BGP sessions, routes, host routes.
    Validate {
        /// Full `swadm`/`mgadm` output instead of summary counts.
        #[arg(long)]
        detail: bool,
    },
}

#[derive(Subcommand)]
enum RackCmd {
    /// Swap a single component on the running rack at a ref, then restart it.
    ///
    /// Fetches the prebuilt artifact from buildomat, sha-verifies it, and places
    /// it on the relevant nodes. Live + ephemeral (a clean relaunch reverts; see
    /// `image patch` to persist). `--list` shows the patchable components.
    Patch {
        /// Component to patch (e.g. propolis, mgd, dendrite, lldp). Omit with
        /// `--list` to see them all.
        component: Option<String>,
        /// Git ref (commit) to patch to - the buildomat image revision.
        reference: Option<String>,
        /// List the patchable components and exit.
        #[arg(long)]
        list: bool,
        /// Print the plan (component, ref, target nodes) without applying it.
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Subcommand)]
enum SpCmd {
    /// List the live SPs over MGS: type, serial, power, archive id.
    ///
    /// Via faux-mgs in the switch zone; needs a running `--emu` rack.
    #[command(visible_alias = "list")]
    Ls {
        /// Which switch zone to query (`switch0`|`switch1`|`<scrimlet>`).
        #[arg(long, default_value = "switch0")]
        switch: String,
    },
    /// Show one SP's state: serial, power, RoT, archive.
    #[command(visible_alias = "state")]
    Info {
        /// Target SP: serial (e.g. BRM44220001), node (sidecar | g0 | g1 ...),
        /// or sim addr ([::1]:33310 | 33310).
        target: String,
        #[arg(long, default_value = "switch0")]
        switch: String,
    },
    /// Show an SP's power state.
    #[command(visible_alias = "st")]
    Status {
        /// Target SP: serial, node (sidecar | g0 ...), or sim addr.
        target: String,
        #[arg(long, default_value = "switch0")]
        switch: String,
    },
    /// Inject an NMI into the host via the SP.
    Nmi {
        /// Target SP: serial, node (sidecar | g0 ...), or sim addr.
        target: String,
        #[arg(long, default_value = "switch0")]
        switch: String,
    },
    /// Pass a raw faux-mgs command to an SP.
    ///
    /// The command's own args follow after `-e`, e.g. `-e inventory`,
    /// `-e read-caboose 0`, `-e dump count`, `-e dump read --index 0`. Everything
    /// after `-e` is passed through (a quoted string works too).
    Exec {
        /// Target SP: serial, node (sidecar | g0 ...), or sim addr.
        target: String,
        #[arg(long, default_value = "switch0")]
        switch: String,
        /// The faux-mgs command + its args (`-e read-caboose 0`).
        #[arg(short = 'e', long = "exec", num_args = 1.., allow_hyphen_values = true)]
        command: Vec<String>,
    },
    /// Check whether `launch --emu` is ready (artifacts present; no rack needed).
    Ready,
    /// Flash a hubris `.zip` into an sp-emu slot-A flash file (offline).
    Flash {
        /// Hubris image archive (e.g. build-gimlet-c-image-default.zip).
        image: PathBuf,
        /// Output flash file.
        out: PathBuf,
    },
    /// Re-flash a live SP (or the shared RoT) and restart its sp-emu service.
    ///
    /// The firmware counterpart to `rack patch`. `<image>` is a hubris `.zip`
    /// for an SP, or a raw oxide-rot-1 image for target `rot` (restarts every RoT
    /// bridge). Live + ephemeral (reverts on relaunch; bake via `build-cp.sh`).
    Reflash {
        /// Target: `sidecar` | `gN` | a port | `rot`.
        target: String,
        /// Hubris `.zip` (SP) or raw oxide-rot-1 flash image (target `rot`).
        image: PathBuf,
        #[arg(long, default_value = "switch0")]
        switch: String,
    },
    /// Enable (or `--off`) the in-zone humility debug listeners (gdb/ocd) for one SP.
    ///
    /// Toggles `SP_EMU_NO_DEBUG` + restarts the SP (~30s preboot). On enable,
    /// prints the humility ports + attach command. Live + ephemeral.
    Debug {
        /// Target SP: `sidecar` | `gN` | a port.
        target: String,
        /// Disable debug (re-suppress the listeners) instead of enabling.
        #[arg(long)]
        off: bool,
        #[arg(long, default_value = "switch0")]
        switch: String,
    },
    /// Force + decode a crash dump of one live emulated SP.
    ///
    /// Arms the SP for dumps (a one-time ~30s restart if needed), triggers a
    /// humility RAM snapshot in-zone, pulls it to the host, and runs `humility
    /// hydrate` + `tasks`/`ringbuf`. Needs `humility` on PATH (or `$VOXEL_HUMILITY`).
    Dump {
        /// Target SP: `sidecar` | `gN` | a port.
        target: String,
        /// Run humility `ringbuf` instead of `tasks` on the hydrated dump.
        #[arg(long)]
        ringbuf: bool,
        #[arg(long, default_value = "switch0")]
        switch: String,
    },
    /// Drive one host<->SP IPCC exchange over the SP's control UART (RFD 316).
    ///
    /// Arms the SP's UART7 with a socket (`SP_EMU_HOST_UART`, a one-time ~30s
    /// restart), plays the host from in-zone, sends a `HostToSp` request, and
    /// decodes the `SpToHost` reply - proving the emulated SP speaks IPCC.
    Ipcc {
        /// Target SP: `sidecar` | `gN` | a port.
        target: String,
        /// Request to send: identity (VPD) | bsu (boot storage unit) | macs |
        /// status (host boot options) | inventory.
        #[arg(long, default_value = "identity")]
        cmd: String,
        #[arg(long, default_value = "switch0")]
        switch: String,
    },
    /// Build the gimlet-c + sidecar-c-emu images from a hubris commit.
    Build {
        /// hubris git commit to build (v1 builds from the configured checkout).
        commit: String,
    },
}

#[derive(Subcommand)]
enum HostCmd {
    /// List sleds and their external IPs.
    Ls,
    /// SSH into a sled's global zone: `voxel host login g1`.
    Login {
        #[arg(default_value = "g0")]
        sled: String,
    },
    /// Run a command in a sled's global zone: `voxel host exec -c "svcs -x" g1`.
    Exec {
        /// Command to run (quote multi-word commands).
        #[arg(short = 'c', long = "command")]
        command: String,
        /// Target sled (g0, g1, ...).
        #[arg(default_value = "g0")]
        sled: String,
    },
}

#[derive(Subcommand)]
enum TpCmd {
    /// List switch zones (scrimlets) and their external IPs.
    Ls,
    /// SSH into a switch zone (technician port): `voxel tp login switch0`.
    ///
    /// Drops you in oxz_switch, where the dendrite/maghemite tools live
    /// (`swadm`, `dpd`, `mgadm`).
    Login {
        #[arg(default_value = "switch0")]
        switch: String,
    },
    /// Run a command in a switch zone: `voxel tp exec -c "swadm link ls" switch0`.
    Exec {
        /// Command to run in oxz_switch (quote multi-word commands).
        #[arg(short = 'c', long = "command")]
        command: String,
        /// Target switch (`switch0` | `switchN` | `rackR/switchS` | scrimlet node).
        #[arg(default_value = "switch0")]
        switch: String,
    },
}

// ---------------------------------------------------------------------------
// Config loading + project-root resolution
// ---------------------------------------------------------------------------

fn config_text(path: &Path) -> anyhow::Result<String> {
    if path.exists() {
        Ok(fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?)
    } else {
        Ok(VoxelConfig::default().to_toml())
    }
}

fn load_config(path: &Path) -> anyhow::Result<VoxelConfig> {
    let text = config_text(path)?;
    VoxelConfig::from_toml(&text).map_err(|e| anyhow!("parse {}: {e}", path.display()))
}

/// Make a path absolute against the current directory.
fn absolutize(p: PathBuf) -> PathBuf {
    if p.is_absolute() {
        p
    } else {
        std::env::current_dir().unwrap_or_default().join(p)
    }
}

/// Discover the `voxel.toml` to use, as an absolute path. Order: explicit
/// `--config`/`$VOXEL_CONFIG` -> the user config `~/.config/voxel/voxel.toml` ->
/// `/etc/voxel/voxel.toml`. The user config is the default *and* where a fresh
/// `config set` writes, so edits and launches always hit the same file no matter
/// the CWD - no implicit project-local `./voxel.toml` to silently diverge from
/// (use `--config` / `config load` for a one-off). Falls back to `./voxel.toml`
/// only if `$HOME` is unset.
fn discover_config(explicit: Option<&Path>) -> PathBuf {
    if let Some(p) = explicit {
        return absolutize(p.to_path_buf());
    }
    let user = std::env::var("HOME")
        .ok()
        .map(|h| PathBuf::from(h).join(".config/voxel/voxel.toml"));
    if let Some(cand) = &user {
        if cand.is_file() {
            return cand.clone();
        }
    }
    let etc = PathBuf::from("/etc/voxel/voxel.toml");
    if etc.is_file() {
        return etc;
    }
    // Nothing exists yet: default to the user config so a fresh `config set`
    // creates it there (only fall back to ./voxel.toml if $HOME is unset).
    user.unwrap_or_else(|| absolutize(PathBuf::from("voxel.toml")))
}

/// Resolve falcon settings (flag > voxel.toml `[falcon]` > existing env) and
/// export them as `FALCON_DATASET` / `VOXEL_RSS_GEN`, so falcon's `Runner`, the
/// RSS renderer, the `image` commands, and any subprocess all see one consistent
/// value. An unset var falls back to its built-in default (falcon's
/// `rpool/falcon`; the renderer's default path).
fn resolve_falcon_env(cli: &Cli, cfg: Option<&VoxelConfig>) {
    let dataset = cli
        .dataset
        .clone()
        .or_else(|| cfg.and_then(|c| c.falcon.dataset.clone()))
        .or_else(|| std::env::var("FALCON_DATASET").ok());
    if let Some(d) = dataset {
        std::env::set_var("FALCON_DATASET", d);
    }
    // Resolve the build root first (cli > config > env), since the rss-gen path is
    // derived from it below. Export it as-is; apply the default only for our derive.
    let build_root = cli
        .build_root
        .as_ref()
        .map(|p| p.display().to_string())
        .or_else(|| cfg.and_then(|c| c.falcon.build_root.clone()))
        .or_else(|| std::env::var("BUILD_ROOT").ok());
    if let Some(b) = &build_root {
        std::env::set_var("BUILD_ROOT", b);
    }
    let build_root_eff = build_root.unwrap_or_else(|| {
        format!("{}/voxel-builds", std::env::var("HOME").unwrap_or_else(|_| "/root".into()))
    });
    // voxel-rss-gen: `--rss-gen` flag or `$VOXEL_RSS_GEN` still override, but by
    // default DERIVE the path from the image's omicron commit so it can never drift
    // from `image.cp` (no `[falcon].rss_gen` knob to mismatch).
    let rss = cli
        .rss_gen
        .as_ref()
        .map(|p| p.display().to_string())
        .or_else(|| std::env::var("VOXEL_RSS_GEN").ok())
        .or_else(|| {
            cfg.and_then(|c| c.image.cp_commit()).map(|commit| {
                format!("{build_root_eff}/omicron-{commit}/target/debug/voxel-rss-gen")
            })
        });
    if let Some(r) = rss {
        std::env::set_var("VOXEL_RSS_GEN", r);
    }
}

/// `chdir` to the project root so the CWD-relative `cargo-bay/` and `.falcon/`
/// resolve correctly no matter where voxel was invoked from (e.g. `/usr/bin`).
/// Root: `--workdir`/`$VOXEL_WORKDIR` > `[falcon].workdir` > the discovered
/// `voxel.toml`'s directory. No-op if the chosen root isn't a directory.
fn anchor_workdir(cli: &Cli, cfg: Option<&VoxelConfig>, config_path: &Path) -> anyhow::Result<()> {
    let root = cli
        .workdir
        .clone()
        .or_else(|| cfg.and_then(|c| c.falcon.workdir.clone()).map(PathBuf::from))
        .or_else(|| config_path.parent().map(Path::to_path_buf));
    if let Some(root) = root {
        if root.is_dir() {
            std::env::set_current_dir(&root)
                .map_err(|e| anyhow!("chdir to workdir {}: {e}", root.display()))?;
        }
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    let cli = Cli::parse();
    // Anchor to the project root before anything touches cargo-bay/.falcon.
    let config_path = discover_config(cli.config.as_deref());
    let cfg = load_config(&config_path).ok();
    resolve_falcon_env(&cli, cfg.as_ref());
    anchor_workdir(&cli, cfg.as_ref(), &config_path)?;
    match &cli.cmd {
        Cmd::Launch { no_progress, no_route, emu_sp, emu_rot, wicket_setup } => {
            rack::cmd_launch(&load_config(&config_path)?, &cli.name, *no_progress, *no_route, *emu_sp || *emu_rot, *emu_rot, *wicket_setup)
                .await
        }
        Cmd::WicketDryrun { config_rss, sleds } => wicket_setup::dryrun(config_rss, *sleds),
        Cmd::Route { dry_run } => {
            rack::cmd_route(&load_config(&config_path)?, &cli.name, *dry_run).await
        }
        Cmd::Destroy => rack::cmd_destroy(&load_config(&config_path)?, &cli.name),
        Cmd::Serial { node } => {
            access::cmd_serial(&load_config(&config_path)?, &cli.name, node).await
        }
        Cmd::Info => rack::cmd_info(&load_config(&config_path)?, &cli.name),
        Cmd::Status => rack::cmd_status(&load_config(&config_path)?, &cli.name).await,
        Cmd::Config { cmd } => config_cmd::cmd_config(&config_path, cmd),
        Cmd::Image { cmd } => match cmd {
            ImageCmd::Patch { component, reference, image, out } => {
                let cfg = load_config(&config_path)?;
                let src = image.clone().unwrap_or_else(|| cfg.image.cp_image());
                patch::cmd_image_patch(component, reference, &src, out.as_deref())
            }
            other => image::cmd_image(other, cfg.as_ref().map(|c| c.image.cp_image())),
        },
        Cmd::Network { cmd } => match cmd {
            NetworkCmd::Show => network::show(&load_config(&config_path)?),
            NetworkCmd::AddPort { a, b } => network::add_port(&config_path, a, b),
            NetworkCmd::RmPort { a, b } => network::rm_port(&config_path, a, b),
            NetworkCmd::LinkUp { switch, port, speed, fec } => {
                network::link_up(&load_config(&config_path)?, &cli.name, switch, port, speed, fec).await
            }
            NetworkCmd::LinkDown { switch, port } => {
                network::link_down(&load_config(&config_path)?, &cli.name, switch, port).await
            }
            NetworkCmd::Validate { detail } => {
                network::validate(&load_config(&config_path)?, &cli.name, *detail).await
            }
        },
        Cmd::Rack { cmd } => match cmd {
            RackCmd::Patch { component, reference, list, dry_run } => {
                if *list {
                    patch::list();
                    Ok(())
                } else {
                    let component = component
                        .as_deref()
                        .ok_or_else(|| anyhow!("missing component (try `voxel rack patch --list`)"))?;
                    let reference = reference
                        .as_deref()
                        .ok_or_else(|| anyhow!("missing ref (usage: voxel rack patch {component} <ref>)"))?;
                    patch::cmd_rack_patch(&load_config(&config_path)?, &cli.name, component, reference, *dry_run)
                        .await
                }
            }
        },
        Cmd::Sp { cmd } => sp_cmd::cmd_sp(&load_config(&config_path)?, &cli.name, cmd).await,
        Cmd::Host { cmd } => match cmd {
            HostCmd::Ls => access::cmd_host_ls(&load_config(&config_path)?, &cli.name).await,
            HostCmd::Login { sled } => {
                access::cmd_host_login(&load_config(&config_path)?, &cli.name, sled).await
            }
            HostCmd::Exec { command, sled } => {
                access::cmd_host_exec(&load_config(&config_path)?, &cli.name, sled, command).await
            }
        },
        Cmd::Tp { cmd } => match cmd {
            TpCmd::Ls => access::cmd_tp_ls(&load_config(&config_path)?, &cli.name).await,
            TpCmd::Login { switch } => {
                access::cmd_tp_login(&load_config(&config_path)?, &cli.name, switch).await
            }
            TpCmd::Exec { command, switch } => {
                access::cmd_tp_exec(&load_config(&config_path)?, &cli.name, switch, command).await
            }
        },
    }
}
