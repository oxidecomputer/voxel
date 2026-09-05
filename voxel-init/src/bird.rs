// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Launch-time BIRD setup for external falcon-lab topologies. Falcon mounts
//! the cargo-bay first; the caller supplies networking rather than inheriting
//! voxel's FRR-specific rack interface ordering and NAT policy.

use crate::sys::{note, run};
use anyhow::{Result, bail};
use camino::Utf8Path;

const READY: &str = "/run/voxel-bird-ready";
const CONFIG: &str = "/etc/bird/bird.conf";

pub fn bring_up(
    config: &Utf8Path,
    init_script: Option<&Utf8Path>,
) -> Result<()> {
    // Falcon exec doesn't return the guest exit status. Never leave an old
    // success marker behind when a subsequent attempt fails.
    if !run("rm", &["-f", READY]) {
        bail!("clearing BIRD readiness marker failed");
    }
    if !config.is_file() {
        bail!("BIRD config not found: {config}");
    }
    if let Some(script) = init_script {
        if !script.is_file() {
            bail!("BIRD init script not found: {script}");
        }
        if !run("bash", &["-e", "--", script.as_str()]) {
            bail!("BIRD init script failed: {script}");
        }
    }

    // Validate before replacing a working configuration. Explicit ownership
    // makes a root-owned/0600 host config readable by Debian's bird service.
    if !run("bird", &["-p", "-c", config.as_str()]) {
        bail!("invalid BIRD config: {config}");
    }
    if !run(
        "install",
        &["-o", "bird", "-g", "bird", "-m", "0640", config.as_str(), CONFIG],
    ) {
        bail!("installing BIRD config failed");
    }
    if !run("systemctl", &["restart", "bird"]) {
        bail!("starting BIRD failed; inspect journalctl -u bird");
    }
    // Debian's Type=simple service can return before the control socket opens.
    for attempt in 0..20 {
        if run("birdc", &["show", "status"]) {
            break;
        }
        if attempt == 19 {
            bail!("BIRD control socket is not responding");
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    if !run("systemctl", &["enable", "bird"]) {
        bail!("enabling BIRD on subsequent boots failed");
    }
    if !run("touch", &[READY]) {
        bail!("writing BIRD readiness marker failed");
    }
    note("BIRD bring-up complete");
    Ok(())
}
