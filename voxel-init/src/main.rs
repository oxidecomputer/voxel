// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! In-guest agent for Voxel images.
//!
//! `gimlet` and `router` configure rack nodes at launch. `install` prepares
//! control-plane, FRR and Helios base images in the builder guest.

mod gimlet;
mod install;
mod router;
mod sys;

use clap::{Parser, Subcommand, ValueEnum};

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
    /// Install software while building an image.
    Install {
        /// Image type to install.
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

/// Image type installed by the builder.
#[derive(Copy, Clone, PartialEq, Eq, ValueEnum)]
enum InstallRole {
    /// voxel-cp control-plane image (illumos / helios).
    Cp,
    /// voxel-frr router image (linux / debian).
    Frr,
    /// Helios base image, updated with `pkg update`.
    Os,
}

fn main() {
    let result = match Cli::parse().cmd {
        Cmd::Gimlet => gimlet::bring_up(),
        Cmd::Router => router::bring_up(),
        Cmd::Install { role } => match role {
            InstallRole::Cp => install::build_control_plane_image(),
            InstallRole::Frr => install::build_frr_image(),
            InstallRole::Os => install::build_os_image(),
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
