//! voxel-init—the in-guest bring-up agent baked into voxel images.
//!
//! Replaces the per-node launch shell scripts (`gimlet-launch.sh`,
//! `router-launch.sh`): `voxel launch` runs `voxel-init <role>` inside each
//! guest. One binary, two roles, cross-compiled for both guest OSes—illumos
//! (the voxel-cp gimlet) and linux (the voxel-frr router). Everything is
//! orchestration of OS commands, so the same source builds for both targets;
//! only the role selected at runtime differs.

mod gimlet;
mod install;
mod router;
mod sys;

use clap::{Parser, Subcommand, ValueEnum};

#[derive(Parser)]
#[command(name = "voxel-init", about = "In-guest bring-up agent for voxel racks")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Bring up a sled from a voxel-cp image (illumos / helios).
    Gimlet,
    /// Bring up a customer router/edge from a voxel-frr image (linux / debian).
    Router,
    /// Image-BUILD-time install, run inside the builder guest by `voxel image bake`.
    /// Installs baked software only; applies no topology configuration.
    Install {
        /// Which image is being baked.
        #[arg(long, value_enum)]
        role: InstallRole,
    },
    /// Internal: the detached switch-config enforcer for a scrimlet's slot
    /// (spawned by `gimlet`)—swaps the launch-count MGS + sp-sim configs in.
    #[command(hide = true)]
    SwitchEnforcer { slot: u8 },
    /// Internal: the baked `svc:/oxide/voxel-switch-enforcer` SMF method. Runs at
    /// every boot, reads this scrimlet's slot from the cargo-bay, and enforces
    /// it—the reboot/restart-safe path (no-op on gimlets / switch0).
    #[command(hide = true)]
    SwitchEnforcerSvc,
}

/// Which image `install` is baking. Both arms build for both guest OSes; the
/// role picks the implementation at runtime, as with `gimlet` / `router`.
#[derive(Copy, Clone, PartialEq, Eq, ValueEnum)]
enum InstallRole {
    /// voxel-cp control-plane image (illumos / helios).
    Cp,
    /// voxel-frr router image (linux / debian).
    Frr,
}

fn main() {
    let result = match Cli::parse().cmd {
        Cmd::Gimlet => gimlet::bring_up(),
        Cmd::Router => router::bring_up(),
        Cmd::Install { role } => match role {
            InstallRole::Cp => install::cp(),
            InstallRole::Frr => install::frr(),
        },
        Cmd::SwitchEnforcer { slot } => {
            gimlet::switch_enforcer(slot);
            Ok(())
        }
        Cmd::SwitchEnforcerSvc => {
            gimlet::switch_enforcer_svc();
            Ok(())
        }
    };
    if let Err(e) = result {
        eprintln!("[voxel-init] FATAL: {e:#}");
        std::process::exit(1);
    }
}
