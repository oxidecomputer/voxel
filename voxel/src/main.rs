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
mod rack;
mod rss;
mod topo;

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

    /// Project root that cargo-bay/ and .falcon/ live under - lets voxel run from
    /// anywhere. Overrides voxel.toml `[falcon].workdir`; default is the
    /// discovered voxel.toml's directory.
    #[arg(long, global = true, env = "VOXEL_WORKDIR")]
    workdir: Option<PathBuf>,

    /// Topology (falcon deployment) name.
    #[arg(long, global = true, default_value = "voxel", env = "VOXEL_NAME")]
    name: String,

    /// zfs dataset falcon uses (overrides voxel.toml `[falcon].dataset` / env;
    /// default `rpool/falcon`).
    #[arg(long, global = true)]
    dataset: Option<String>,

    /// Path to the commit-pinned `voxel-rss-gen` (overrides `[falcon].rss_gen` /
    /// env).
    #[arg(long, global = true)]
    rss_gen: Option<PathBuf>,

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
    let rss = cli
        .rss_gen
        .as_ref()
        .map(|p| p.display().to_string())
        .or_else(|| cfg.and_then(|c| c.falcon.rss_gen.clone()))
        .or_else(|| std::env::var("VOXEL_RSS_GEN").ok());
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
        Cmd::Launch { no_progress, no_route } => {
            rack::cmd_launch(&load_config(&config_path)?, &cli.name, *no_progress, *no_route).await
        }
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
        Cmd::Image { cmd } => image::cmd_image(cmd),
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
