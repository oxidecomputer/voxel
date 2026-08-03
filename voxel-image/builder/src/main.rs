use anyhow::{Error, anyhow};
use clap::Args;
use libfalcon::{
    Runner,
    cli::{RunMode, run_with_extra},
    unit::gb,
};
use slog::info;

// PROTOTYPE (Project Voxel, snapshot-first image path).
//
// A single-node "builder" topology, generic over base image + payload so it can
// bake any Voxel image. On `launch` it boots VBUILD_IMAGE (default helios-3.0),
// mounts VBUILD_CARGO_BAY at /opt/cargo-bay, and runs INSTALL_SCRIPT (default
// install-cp.sh) - which installs baked software WITHOUT applying any
// topology-specific config. build-image.sh then captures the disk.
//
//   voxel-cp  : VBUILD_IMAGE=helios-3.0  INSTALL_SCRIPT=install-cp.sh
//   voxel-frr : VBUILD_IMAGE=debian-13.2 INSTALL_SCRIPT=install-frr.sh
//
// We get launch / exec / hyperstop / destroy / serial for free from falcon's
// CLI via run_with_extra. We register no extra subcommands.

/// No topology-specific subcommands; falcon's built-ins are all we need.
#[derive(Args)]
struct Extra {}

async fn extra(_r: &mut Runner, _opts: Extra) -> Result<(), libfalcon::error::Error> {
    Ok(())
}

const HELIOS_IMG: &str = "helios-3.0";
const NODE: &str = "vbuild";

fn env_u8(key: &str, default: u8) -> u8 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    // Deployment name; must match DEPLOY in build-image.sh so the capture step
    // can find the node's zvol at <dataset>/topo/<name>/vbuild.
    // falcon deployment names must match ^[A-Za-z]?[A-Za-z0-9_]*$ (no hyphens).
    let name = std::env::var("VOXEL_BUILD_NAME").unwrap_or_else(|_| "voxel_build".to_string());
    let mut d = Runner::new(&name);

    let cores = env_u8("VBUILD_CORES", 8);
    let mem_gb = env_u64("VBUILD_MEM_GB", 16);
    let disk_gb = env_u64("VBUILD_DISK_GB", 100) as usize;

    // Base image to boot. Defaults to stock helios for an image BUILD; set
    // VBUILD_IMAGE=voxel-cp-<ver> to boot a node FROM a previously built image
    // (smoke test / validation).
    let image = std::env::var("VBUILD_IMAGE").unwrap_or_else(|_| HELIOS_IMG.to_string());
    let vbuild = d.node(NODE, &image, cores, gb(mem_gb));
    d.reserve(vbuild, disk_gb);

    // External link so the builder can reach pkg.oxide.computer during install.
    if let Ok(ifx) = std::env::var("EXT_INTERFACE") {
        d.ext_link(&ifx, vbuild);
    } else {
        d.default_ext_link(vbuild)
            .map_err(|e| anyhow!("failed to find default external interface: {e}"))?;
    }

    // Stage the install payload (the cargo-bay holding INSTALL_SCRIPT + any
    // artifacts it needs, e.g. omicron for install-cp.sh).
    let cargo_bay =
        std::env::var("VBUILD_CARGO_BAY").unwrap_or_else(|_| "./cargo-bay/vbuild".to_string());
    // illumos guests use mount(); linux guests need mount_linux() (the guest-side
    // share mechanism differs). Pick based on the base image.
    let is_linux =
        image.starts_with("debian") || image.starts_with("ubuntu") || image.starts_with("linux");
    let mounted = if is_linux {
        d.mount_linux(&cargo_bay, "/opt/cargo-bay", vbuild)
    } else {
        d.mount(&cargo_bay, "/opt/cargo-bay", vbuild)
    };
    mounted.map_err(|e| anyhow!("mount cargo-bay ({cargo_bay}): {e}"))?;

    if let RunMode::Launch = run_with_extra(&mut d, extra).await? {
        // VBUILD_SKIP_INSTALL=1 boots the node and runs nothing - used to smoke
        // test that a captured image boots with its payload intact.
        if std::env::var("VBUILD_SKIP_INSTALL").is_ok() {
            info!(
                d.log,
                "VBUILD_SKIP_INSTALL set; booted from image {}, skipping install", image
            );
        } else {
            let script =
                std::env::var("INSTALL_SCRIPT").unwrap_or_else(|_| "install-cp.sh".to_string());
            // Invoke via `bash` rather than `./{script}` + chmod: linux guests
            // get a read-only 9p mount where chmod fails (and the host already
            // stages the script executable).
            let install_cmd =
                format!("cd /opt/cargo-bay && bash ./{script} 2>&1 | tee /tmp/install.log");
            info!(d.log, "running install script {}", script);
            d.exec(vbuild, &install_cmd)
                .await
                .map_err(|e| anyhow!("install ({script}): {e}"))?;
            info!(
                d.log,
                "install ({}) complete; node ready for capture", script
            );
        }
    }

    Ok(())
}
