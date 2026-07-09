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
    /// Path to voxel.toml. Unset -> the user config ~/.config/voxel/voxel.toml,
    /// then /etc/voxel/voxel.toml. Use this flag (or `config load`) for a one-off.
    #[arg(long, global = true, env = "VOXEL_CONFIG")]
    config: Option<PathBuf>,

    /// Project root that cargo-bay/ and .falcon/ live under 
    #[arg(long, global = true, env = "VOXEL_WORKDIR")]
    workdir: Option<PathBuf>,

    /// Topology (falcon deployment) name.
    #[arg(long, global = true, default_value = "voxel", env = "VOXEL_NAME")]
    name: String,

    /// zfs dataset falcon uses (overrides voxel.toml `[falcon].dataset` / env;
    /// default `rpool/falcon`).
    #[arg(long, global = true)]
    dataset: Option<String>,

    /// Path to the commit-pinned `voxel-rss-gen`. Normally derived from the image's
    /// omicron commit (`<build_root>/omicron-<commit>/...`); this + `$VOXEL_RSS_GEN`
    /// override it for one-offs.
    #[arg(long, global = true)]
    rss_gen: Option<PathBuf>,

    /// Build root for `voxel image create` (overrides `[falcon].build_root` /
    /// `$BUILD_ROOT`; default `$HOME/voxel-builds`).
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
        /// Run real-firmware SPs on the `sp-emu` emulator (whole fleet) instead of
        /// `sp-sim`. Needs `[sp].emu_bin` + the hubris images in `[sp]`. Default: sp-sim.
        #[arg(long)]
        emu_sp: bool,
        /// Additionally wire the RoT bridge (oxide-rot-1) onto the sidecar SP.
        /// Implies --emu-sp. Needs [sp].rot_image. Runs the sidecar as two
        /// emulated cores - keep OFF during initial bring-up (wedges handoff).
        #[arg(long = "emu-rot")]
        emu_rot: bool,
        /// Drive rack setup THROUGH wicketd (the real operator flow) instead of
        /// the file-based sled-agent auto-init: suppresses the staged config-rss
        /// so sled-agent waits, then uploads the config + a self-signed cert +
        /// the recovery password to wicketd and POSTs to start RSS. Fully
        /// populates wicket's RACK SETUP page. (Uses the progress path.)
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
    /// Run a command on a node: `voxel exec g0 svcs -x`.
    Exec {
        node: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        command: Vec<String>,
    },
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
    /// Manage real-firmware SP-emulator (`sp-emu`) artifacts for `launch --emu`.
    Sp {
        #[command(subcommand)]
        cmd: SpCmd,
    },
    /// Log into a sled's global zone.
    Host {
        #[command(subcommand)]
        cmd: HostCmd,
    },
    /// Log into a switch zone (technician port).
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
    /// Build a `voxel-cp` image for an omicron commit (builds omicron from
    /// source - TUF lacks the i86pc global-zone software, see roadmap).
    Create {
        /// omicron git commit (or tag) to build and pin the image to.
        commit: String,
    },
    /// Export an image bundle to a file for distribution. Defaults to a `zfs
    /// send | zstd` stream (`<name>.zfs.zst`); `--raw` makes a portable
    /// `<name>.raw.xz` disk image instead.
    Export {
        /// Image name (e.g. `voxel-cp-a3fee0ec`).
        name: String,
        /// Output file (default `<name>.zfs.zst`, or `<name>.raw.xz` with --raw).
        out: Option<PathBuf>,
        /// Portable raw disk image (`dd | xz`) instead of a zfs stream.
        #[arg(long)]
        raw: bool,
    },
    /// Import an image bundle from a file (`.zfs.zst` or `.raw.xz`) produced by
    /// `image export`, into `<dataset>/img/<name>@base`.
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
    /// Fold a component patch into the image's @base so it persists across
    /// relaunches (boot-modify-capture: boots the source image, places the
    /// artifact in-guest, re-captures). Durable counterpart to `rack patch`;
    /// slower (~minutes) but survives a clean relaunch. propolis + ddm-gz only
    /// for now (switch-zone services need a switch.tar.gz repack).
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
    /// Add a switch-to-switch interconnect (a direct sidecar<->sidecar QSFP link
    /// carrying the underlay) between two switches; applied on the next launch.
    /// Selectors: `switch0` | `switch1` | `switchN` | `rackR/switchS`.
    AddPort { a: String, b: String },
    /// Remove a switch interconnect (either order); applied on the next launch.
    RmPort { a: String, b: String },
    /// (debug/transient) Bring up a switch port's link on a RUNNING rack via
    /// `swadm`: create (if needed) + enable the link in the switch zone, e.g.
    /// `voxel network link-up switch0 qsfp2` for the interconnect. Run on both
    /// ends - the link reaches Up once both are enabled. ⚠️ Nexus's reconciler
    /// reaps manual swadm links in ~30s; persistent config must use the Oxide API.
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
    /// Validate live networking on a running rack: per switch zone the link
    /// states, BGP sessions, and programmed routes, plus the host routes.
    Validate {
        /// Show the full `swadm`/`mgadm` output (links, switch ports, bgp,
        /// routes) in long form instead of summary counts.
        #[arg(long)]
        detail: bool,
    },
}

#[derive(Subcommand)]
enum RackCmd {
    /// Swap a single component on the running rack at a given ref (commit), then
    /// restart its service. Fetches the prebuilt artifact from buildomat,
    /// sha-verifies it, and places it on the relevant nodes. Live + ephemeral: a
    /// clean relaunch reverts to the image (see `voxel image patch` to persist).
    /// `voxel rack patch --list` shows the patchable components.
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
    /// List the live SPs over MGS (pilot `sp list`): type, serial, power, archive
    /// id - via `faux-mgs` in the switch zone. Needs a running `--emu` rack.
    #[command(visible_alias = "list")]
    Ls {
        /// Which switch zone to query (`switch0`|`switch1`|`<scrimlet>`).
        #[arg(long, default_value = "switch0")]
        switch: String,
    },
    /// Ask one SP for its state (pilot `sp info`): serial, power, RoT, archive.
    #[command(visible_alias = "state")]
    Info {
        /// Target SP: serial (e.g. BRM44220001), node (sidecar | g0 | g1 ...),
        /// or sim addr ([::1]:33310 | 33310).
        target: String,
        #[arg(long, default_value = "switch0")]
        switch: String,
    },
    /// Get an SP's power state (pilot `sp status`).
    #[command(visible_alias = "st")]
    Status {
        /// Target SP: serial, node (sidecar | g0 ...), or sim addr.
        target: String,
        #[arg(long, default_value = "switch0")]
        switch: String,
    },
    /// Inject an NMI into the host via the SP (pilot `sp nmi`).
    Nmi {
        /// Target SP: serial, node (sidecar | g0 ...), or sim addr.
        target: String,
        #[arg(long, default_value = "switch0")]
        switch: String,
    },
    /// Pass a raw faux-mgs command to an SP (pilot `sp exec -e`), e.g.
    /// `voxel sp exec g0 -e inventory`. Full surface: inventory,
    /// component-details, read-sensor-value, dump, read-caboose, rot-boot-info, ...
    Exec {
        /// Target SP: serial, node (sidecar | g0 ...), or sim addr.
        target: String,
        #[arg(long, default_value = "switch0")]
        switch: String,
        /// The faux-mgs command to run (pilot-style single command string).
        #[arg(short = 'e', long = "exec")]
        command: String,
    },
    /// Show the configured sp-emu build artifacts and whether `launch --emu` is
    /// ready (pre-launch readiness check; no running rack needed).
    Ready,
    /// Flash a hubris image (`.zip`) into an sp-emu slot-A flash file (offline:
    /// produces a flash file on disk; see `reflash` to swap one on a live rack).
    Flash {
        /// Hubris image archive (e.g. build-gimlet-c-image-default.zip).
        image: PathBuf,
        /// Output flash file.
        out: PathBuf,
    },
    /// Re-flash a live SP (or the shared RoT) on a running rack and restart its
    /// sp-emu service - the firmware counterpart to `voxel rack patch`. SP
    /// target: `sidecar` | `g0` | `g1` ... | a port; `rot` reflashes the shared
    /// RoT image (restarts every RoT bridge). `<image>` is a hubris `.zip` for an
    /// SP, or a raw oxide-rot-1 flash image for `rot`. Live + ephemeral (reverts
    /// on a clean relaunch); to persist, bake it in via `build-cp.sh` + relaunch.
    Reflash {
        /// Target: `sidecar` | `gN` | a port | `rot`.
        target: String,
        /// Hubris `.zip` (SP) or raw oxide-rot-1 flash image (target `rot`).
        image: PathBuf,
        #[arg(long, default_value = "switch0")]
        switch: String,
    },
    /// Enable (or `--off` to disable) the in-zone humility debug listeners
    /// (gdb/ocd) for one SP - toggles `SP_EMU_NO_DEBUG` on its sp-emu service +
    /// restarts it. On enable, prints the per-SP humility ports + attach command;
    /// the SP reboots (~30s preboot) before the listeners are up. Live +
    /// ephemeral (a clean relaunch reverts to debug-off).
    Debug {
        /// Target SP: `sidecar` | `gN` | a port.
        target: String,
        /// Disable debug (re-suppress the listeners) instead of enabling.
        #[arg(long)]
        off: bool,
        #[arg(long, default_value = "switch0")]
        switch: String,
    },
    /// Build the gimlet-c + sidecar-c-emu v25 images from a hubris commit (via
    /// build-sp.sh), then print the `[sp]` paths to set.
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
}

#[derive(Subcommand)]
enum TpCmd {
    /// List switch zones (scrimlets) and their external IPs.
    Ls,
    /// SSH into a switch zone (technician port): `voxel tp login switch0`.
    /// Drops you in oxz_switch, where the dendrite/maghemite tools live
    /// (`swadm`, `dpd`, `mgadm`). (Real `pilot` isn't in omicron's sim build.)
    Login {
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
        Cmd::Exec { node, command } => {
            access::cmd_exec(&load_config(&config_path)?, &cli.name, node, command).await
        }
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
            other => image::cmd_image(other),
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
        },
        Cmd::Tp { cmd } => match cmd {
            TpCmd::Ls => access::cmd_tp_ls(&load_config(&config_path)?, &cli.name).await,
            TpCmd::Login { switch } => {
                access::cmd_tp_login(&load_config(&config_path)?, &cli.name, switch).await
            }
        },
    }
}
