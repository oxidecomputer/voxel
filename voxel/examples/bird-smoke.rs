// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! One-node, offline BIRD smoke test. Run on Helios in a separate working
//! directory (Falcon owns `.falcon/` there); see docs/bird-image.md.

use anyhow::{Result, ensure};
use camino::Utf8PathBuf;
use clap::Parser;
use libfalcon::{Runner, unit::gb};

#[derive(Parser)]
struct Args {
    #[arg(long, default_value = "voxel-bird-proto")]
    image: String,
    /// Directory containing bird.conf and init.sh.
    #[arg(long)]
    cargo_bay: Utf8PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    // Do not destroy a live topology's workspace, even on the error path.
    ensure!(!std::path::Path::new(".falcon").exists(), "use a fresh workdir");
    let mut d = Runner::new("bird_smoke");
    let bird = d.node("bird1", &args.image, 2, gb(2));
    d.reserve(bird, 20);
    d.mount_linux(args.cargo_bay.as_str(), "/opt/cargo-bay", bird)?;
    // No external link: installing packages at launch cannot make this pass.
    let result: Result<()> = async {
        d.launch().await?;
        let baked = d.exec(bird, "cat /var/voxel-image-ready").await?;
        ensure!(baked.contains("voxel-bird version="), "not a BIRD image: {baked}");

        // Reapply too, to exercise the same base image with launch-time inputs.
        for _ in 0..2 {
            let log = d
                .exec(
                    bird,
                    "/opt/oxide/voxel-init bird --init-script /opt/cargo-bay/init.sh",
                )
                .await?;
            println!("{log}");
            // exec's Result reports transport errors, not guest exit status.
            let status = d
                .exec(
                    bird,
                    "test -f /run/voxel-bird-ready && birdc show status && \
                     birdc show protocols && echo voxel-bird-smoke-ok",
                )
                .await?;
            ensure!(
                status.lines().any(|line| line.trim() == "voxel-bird-smoke-ok"),
                "BIRD initialization failed: {status}"
            );
            println!("{status}");
        }
        Ok(())
    }
    .await;
    let cleanup = d.destroy();
    result?;
    cleanup?;
    println!("BIRD offline smoke test passed");
    Ok(())
}
