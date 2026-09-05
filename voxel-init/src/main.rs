// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! voxel-init—the in-guest bring-up agent baked into voxel images.
//!
//! Replaces the per-node launch shell scripts (`gimlet-launch.sh`,
//! `router-launch.sh`): `voxel launch` runs `voxel-init <role>` inside each
//! guest. One binary, cross-compiled for both guest OSes—illumos
//! (the voxel-cp gimlet) and linux (FRR and BIRD routers). Everything is
//! orchestration of OS commands, so the same source builds for both targets;
//! only the role selected at runtime differs.

mod bird;
mod gimlet;
mod install;
mod router;
mod sys;

use camino::Utf8PathBuf;
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
    /// Apply a mounted config to a voxel-bird guest, without installing packages.
    Bird {
        /// BIRD 2 configuration to copy to /etc/bird/bird.conf.
        #[arg(long, default_value = "/opt/cargo-bay/bird.conf")]
        config: Utf8PathBuf,
        /// Optional trusted Bash script to set up interfaces before starting BIRD.
        /// Runs as root with `bash -e`; need not be executable on the 9p mount.
        #[arg(long)]
        init_script: Option<Utf8PathBuf>,
    },
    /// Image-BUILD-time install, run inside the builder guest by `image bake`.
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
    /// voxel-bird router image (linux / debian, BIRD 2).
    Bird,
}

fn main() {
    let result = match Cli::parse().cmd {
        Cmd::Gimlet => gimlet::bring_up(),
        Cmd::Router => router::bring_up(),
        Cmd::Bird { config, init_script } => {
            bird::bring_up(&config, init_script.as_deref())
        }
        Cmd::Install { role } => match role {
            InstallRole::Cp => install::build_control_plane_image(),
            InstallRole::Frr => install::build_frr_image(),
            InstallRole::Bird => install::build_bird_image(),
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
