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

use anyhow::{Context, Error};
use clap::{Parser, Subcommand};
use std::fs;
use std::path::{Path, PathBuf};
use voxel_config::VoxelConfig;

mod access;
mod commtest;
mod config_cmd;
mod cpbuild;
mod image;
mod imagebuild;
mod isolated_external;
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
    /// Build and run Omicron's commit-matched end-to-end connectivity test.
    ///
    /// Arguments after `--` are passed directly to commtest. If no command is
    /// supplied, defaults to `run`; Voxel supplies the rack API and a test IP
    /// pool unless those are explicitly overridden.
    Commtest {
        /// Omicron commit/tag to test, or `main` for the latest upstream.
        /// Omit to use the commit encoded in the configured control-plane image.
        #[arg(value_name = "COMMIT")]
        reference: Option<String>,

        /// Use an existing Omicron checkout without fetching or changing it.
        #[arg(long, value_name = "PATH", conflicts_with = "reference")]
        source: Option<PathBuf>,

        /// Rack to target (1-based).
        #[arg(long, default_value_t = 1)]
        rack: usize,

        /// Override the derived Nexus API URL.
        #[arg(long, value_name = "URL")]
        api: Option<String>,

        /// Connectivity phase to run (`uni` and `multi` are accepted aliases).
        #[arg(long, value_enum, default_value_t = commtest::Traffic::Unicast)]
        traffic: commtest::Traffic,

        /// Run an already-built commtest binary.
        #[arg(long)]
        no_build: bool,

        /// Permit running with effective uid 0. Build artifacts and reports
        /// under the build root become root-owned, which later unprivileged
        /// runs may trip over.
        #[arg(long)]
        allow_root: bool,

        /// Arguments passed to Omicron commtest (place them after `--`).
        #[arg(last = true, allow_hyphen_values = true)]
        args: Vec<String>,
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
    ///
    /// `--src <path>` instead builds an existing omicron checkout/worktree AS-IS
    /// (the dev loop: your working-tree edits, warm target).
    Create {
        /// omicron git commit (or tag) to build and pin the image to. With
        /// `--src` this is an optional image label (default: the checkout's HEAD).
        #[arg(required_unless_present = "src")]
        commit: Option<String>,
        /// Build from an existing omicron checkout/worktree AS-IS (host build,
        /// for dev): skips clone + checkout so your working-tree edits are built.
        /// Applies voxel's omicron patches + smf configs to that tree (idempotent).
        #[arg(long)]
        src: Option<PathBuf>,
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
    /// Build a `voxel-frr` customer-router image.
    CreateFrr {
        /// Image label; the image is named `voxel-frr-<version>`.
        #[arg(default_value = "proto")]
        version: String,
    },
    /// (build helper) Bake an image: boot a one-node builder, run the in-guest
    /// agent's install role, capture the disk. Used by build-cp.sh/build-frr.sh.
    #[command(hide = true)]
    Bake {
        /// Registered image name (captured to `<dataset>/img/<name>@base`).
        name: String,
        /// Base image the builder boots.
        #[arg(long, default_value = "helios-3.0")]
        base: String,
        /// Agent install role (`cp` | `frr`).
        #[arg(long)]
        role: Option<String>,
        /// An in-guest command to run instead of an agent role
        /// (boot-modify-capture, used by `image patch`). With neither, the
        /// builder just boots, smoke-testing that the image comes up.
        #[arg(long, conflicts_with = "role")]
        exec: Option<String>,
        /// Host dir mounted at `/opt/cargo-bay` in the guest.
        #[arg(long, default_value = "./cargo-bay/vbuild")]
        cargo_bay: PathBuf,
        #[arg(long, default_value_t = 8)]
        cores: u8,
        #[arg(long, default_value_t = 16)]
        mem_gb: u64,
        #[arg(long, default_value_t = 100)]
        disk_gb: u64,
        /// falcon deployment name for the builder topology.
        #[arg(long, default_value = "voxel_build")]
        deploy: String,
        /// Host link the builder reaches the package repos through (default:
        /// falcon's default external interface).
        #[arg(long)]
        ext_interface: Option<String>,
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
    /// Show the per-rack network projection, switches, and the auto cross-rack
    /// sidecar interconnect mesh.
    Show,
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
    LinkDown { switch: String, port: String },
    /// Validate live networking: link states, BGP sessions, routes, host routes.
    Validate {
        /// Full `swadm`/`mgadm` output instead of summary counts.
        #[arg(long)]
        detail: bool,
    },
    /// Manage the isolated ("fake") external segment (`[external] mode = "isolated"`).
    External {
        #[command(subcommand)]
        cmd: ExternalCmd,
    },
}

#[derive(Subcommand)]
enum ExternalCmd {
    /// Stand the segment up (the same path `launch` uses).
    Up {
        /// Print the host commands instead of running them.
        #[arg(long)]
        dry_run: bool,
    },
    /// Tear the segment down (VNIC + etherstub ~ the ipnat rule and
    /// ipv4-forwarding stay).
    Down {
        /// Print the host commands instead of running them.
        #[arg(long)]
        dry_run: bool,
    },
    /// Assert the whole path is live (uplink, links, NAT); PASS/FAIL per item.
    Check,
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
    /// SSH into a sled's global zone or a router: `voxel host login g1`.
    Login {
        #[arg(default_value = "g0")]
        node: String,
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
    VoxelConfig::from_toml(&text).with_context(|| format!("parse {}", path.display()))
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
    if let Some(cand) = &user
        && cand.is_file()
    {
        return cand.clone();
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
/// export them as `FALCON_DATASET` / `BUILD_ROOT` / `VOXEL_OMICRON_SRC`, so
/// falcon's `Runner`, the sled-schema detection, the `image` commands, and any
/// subprocess all see one consistent value. An unset var falls back to its
/// built-in default (falcon's `rpool/falcon`).
fn resolve_falcon_env(cli: &Cli, cfg: Option<&VoxelConfig>) {
    let dataset = cli
        .dataset
        .clone()
        .or_else(|| cfg.and_then(|c| c.falcon.dataset.clone()))
        .or_else(|| std::env::var("FALCON_DATASET").ok());
    if let Some(d) = dataset {
        // SAFETY: runs before the tokio runtime spawns any worker, while the
        // process is still single-threaded, so no concurrent getenv (from
        // Rust or C) can race the write.
        unsafe {
            std::env::set_var("FALCON_DATASET", d);
        }
    }
    // Resolve the build root first (cli > config > env), since the omicron source
    // path is derived from it below. Export it as-is; apply the default only for
    // our derive.
    let build_root = cli
        .build_root
        .as_ref()
        .map(|p| p.display().to_string())
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
    // The omicron checkout the CP image was built from. Only the sled-agent
    // schema detection reads it. Derived from the image's commit so it can't
    // drift from `image.cp`; $VOXEL_OMICRON_SRC overrides.
    let omicron_src = std::env::var("VOXEL_OMICRON_SRC").ok().or_else(|| {
        cfg.and_then(|c| c.image.cp_commit())
            .map(|commit| format!("{build_root_eff}/omicron-{commit}"))
    });
    if let Some(s) = omicron_src {
        // SAFETY: same single-threaded argument as FALCON_DATASET above.
        unsafe {
            std::env::set_var("VOXEL_OMICRON_SRC", s);
        }
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
        .or_else(|| {
            cfg.and_then(|c| c.falcon.workdir.clone())
                .map(PathBuf::from)
        })
        .or_else(|| config_path.parent().map(Path::to_path_buf));
    if let Some(root) = root
        && root.is_dir()
    {
        std::env::set_current_dir(&root)
            .with_context(|| format!("chdir to workdir {}", root.display()))?;
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
        Cmd::Launch {
            no_progress,
            no_route,
            emu_sp,
            emu_rot,
            wicket_setup,
        } => {
            rack::cmd_launch(
                &load_config(&config_path)?,
                &cli.name,
                *no_progress,
                *no_route,
                *emu_sp || *emu_rot,
                *emu_rot,
                *wicket_setup,
            )
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
                // clap rejects <COMMIT> together with --source (conflicts_with).
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
            ImageCmd::Patch {
                component,
                reference,
                image,
                out,
            } => {
                let cfg = load_config(&config_path)?;
                let src = image.clone().unwrap_or_else(|| cfg.image.cp_image());
                patch::cmd_image_patch(component, reference, &src, out.as_deref())
            }
            ImageCmd::Create { commit, src } => {
                cpbuild::create(
                    commit.as_deref(),
                    src.as_deref(),
                    &image::falcon_dataset(),
                    cfg.as_ref(),
                )
                .await
            }
            ImageCmd::CreateFrr { version } => {
                imagebuild::create_frr(version, &image::falcon_dataset(), cfg.as_ref()).await
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
                let bay = cargo_bay.display().to_string();
                let dataset = image::falcon_dataset();
                imagebuild::bake(imagebuild::BakeOpts {
                    base_image: base,
                    role: role.as_deref(),
                    exec: exec.as_deref(),
                    cargo_bay: &bay,
                    image_name: name,
                    dataset: &dataset,
                    deploy,
                    disk_gb: *disk_gb,
                    mem_gb: *mem_gb,
                    cores: *cores,
                    ext_interface: ext_interface.as_deref(),
                    builder_net: None,
                })
                .await
            }
            other => image::cmd_image(other, cfg.as_ref().map(|c| c.image.cp_image())),
        },
        Cmd::Network { cmd } => match cmd {
            NetworkCmd::Show => network::show(&load_config(&config_path)?),
            NetworkCmd::LinkUp {
                switch,
                port,
                speed,
                fec,
            } => {
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
                network::link_down(&load_config(&config_path)?, &cli.name, switch, port).await
            }
            NetworkCmd::Validate { detail } => {
                network::validate(&load_config(&config_path)?, &cli.name, *detail).await
            }
            NetworkCmd::External { cmd } => {
                let cfg = load_config(&config_path)?;
                match cmd {
                    ExternalCmd::Up { dry_run } => isolated_external::up(&cfg.external, *dry_run),
                    ExternalCmd::Down { dry_run } => {
                        isolated_external::down(&cfg.external, *dry_run)
                    }
                    ExternalCmd::Check => isolated_external::check(&cfg.external),
                }
            }
        },
        Cmd::Rack { cmd } => match cmd {
            RackCmd::Patch {
                component,
                reference,
                list,
                dry_run,
            } => {
                if *list {
                    patch::list();
                    Ok(())
                } else {
                    let component = component
                        .as_deref()
                        .context("missing component (try `voxel rack patch --list`)")?;
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
        Cmd::Sp { cmd } => sp_cmd::cmd_sp(&load_config(&config_path)?, &cli.name, cmd).await,
        Cmd::Host { cmd } => match cmd {
            HostCmd::Ls => access::cmd_host_ls(&load_config(&config_path)?, &cli.name).await,
            HostCmd::Login { node } => {
                access::cmd_host_login(&load_config(&config_path)?, &cli.name, node).await
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
