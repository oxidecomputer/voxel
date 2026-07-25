//! voxel-init - the in-guest bring-up agent baked into voxel images.
//!
//! Replaces the per-node launch shell scripts (`gimlet-launch.sh`,
//! `router-launch.sh`): `voxel launch` runs `voxel-init <role>` inside each
//! guest. One binary, two roles, cross-compiled for both guest OSes - illumos
//! (the voxel-cp gimlet) and linux (the voxel-frr router). Everything is
//! orchestration of OS commands, so the same source builds for both targets;
//! only the role selected at runtime differs.

mod gimlet;
mod router;
mod sys;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "voxel-init",
    about = "In-guest bring-up agent for voxel racks"
)]
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
    /// Internal: the detached switch-config enforcer for a scrimlet's slot
    /// (spawned by `gimlet`) - swaps the launch-count MGS + sp-sim configs in.
    #[command(hide = true)]
    SwitchEnforcer { slot: u8 },
    /// Internal: the baked `svc:/oxide/voxel-switch-enforcer` SMF method. Runs at
    /// every boot, reads this scrimlet's slot from the cargo-bay, and enforces it
    /// - the reboot/restart-safe path (no-op on gimlets / switch0).
    #[command(hide = true)]
    SwitchEnforcerSvc,
}

fn main() {
    let result = match Cli::parse().cmd {
        Cmd::Gimlet => gimlet::bring_up(),
        Cmd::Router => router::bring_up(),
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
