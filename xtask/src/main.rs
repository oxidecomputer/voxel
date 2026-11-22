//! Repo automation, run as `cargo xtask <command>`.

mod dist;
mod voxel_up;

use anyhow::Result;
use camino::{Utf8Path, Utf8PathBuf};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "cargo xtask")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Build the release archive CI publishes: voxel, voxel-init,
    /// voxel-init-linux and the pinned sp-emu, each with a .sha256.txt, plus
    /// voxel.tar.gz bundling all four and a voxel-up.sh installer.
    ///
    /// Runs on a Helios host; the native binaries are illumos builds.
    Dist {
        /// Directory the artifacts are written to.
        #[arg(long, default_value = "out/dist")]
        out_dir: Utf8PathBuf,
        /// Commit the rendered voxel-up.sh fetches by default (default: HEAD).
        #[arg(long)]
        rev: Option<String>,
    },
    /// Render voxel-up.sh, a `curl | sh` installer that fetches one commit's CI
    /// artifacts (or installs a local archive) into ./.voxel and writes a
    /// voxel.toml pointing at them.
    VoxelUp {
        /// voxel commit whose CI artifacts the script fetches (default: HEAD).
        /// CI must have built it.
        #[arg(long)]
        rev: Option<String>,
        /// buildomat release series the artifacts are published under.
        #[arg(long, default_value = voxel_up::RELEASE_URL)]
        base_url: String,
        /// Output file; `-` writes to stdout.
        #[arg(short, long, default_value = "voxel-up.sh")]
        out: Utf8PathBuf,
    },
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Dist { out_dir, rev } => dist::dist(&out_dir, rev.as_deref()),
        Cmd::VoxelUp { rev, base_url, out } => {
            let rev = voxel_up::resolve_rev(rev.as_deref().unwrap_or("HEAD"))?;
            voxel_up::write(&rev, &base_url, &out)
        }
    }
}

/// The workspace root, which every cargo and git command runs from.
fn workspace_root() -> &'static Utf8Path {
    Utf8Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask lives in the workspace root")
}
