//! `voxel mupdate` - stage recovery / host-phase-2 artifacts into the live rack's
//! real MGS so the faithful mupdate chain can be exercised: a recovering host's
//! `GetPhase2Data` (relayed by the real SP to the real MGS) is answered from the
//! image cached here. This is the MGS-facing half of the chain; the host side is
//! played by `voxel sp ipcc <sp> --cmd get-phase2:<hash>` until propolis boots a
//! real host. See docs/voxel-mupdate-plan.md.

use std::path::Path;

use anyhow::{anyhow, Result};

use voxel_config::VoxelConfig;

use crate::net::{scp_to, ssh_capture, zlogin, SWITCH_ZONE_ROOT};
use crate::sp_cmd::{clear_cached_ip, switch_ip};
use crate::topo::build_topo;

/// MGS's dropshot API port in the switch zone (omicron `MGS_PORT`).
const MGS_PORT: u16 = 12225;

/// The gateway (MGS) dropshot API is versioned; requests must carry this header
/// (latest of gateway-api's `api_versions!`, tracks the image's omicron).
const GATEWAY_API_VERSION: &str = "3.0.0";

/// `voxel mupdate stage <image>` - POST a host phase-2 image to MGS's recovery
/// cache (`POST /recovery/host-phase2`). MGS keys the cache by sha256 and returns
/// that hash; any SP that later relays `GetPhase2Data{hash}` is served from it.
pub(crate) async fn cmd_stage(
    cfg: &VoxelConfig,
    name: &str,
    switch: &str,
    image: &Path,
    mgs_override: Option<&str>,
) -> Result<()> {
    if !image.exists() {
        return Err(anyhow!("image not found: {}", image.display()));
    }
    let local = image.to_str().ok_or_else(|| anyhow!("non-utf8 image path"))?;
    let topo = build_topo(cfg, name)?;
    let (_fleet, ip, sw) = switch_ip(&topo, switch).await?;

    let remote_img = format!("{SWITCH_ZONE_ROOT}/var/tmp/voxel-phase2.img");
    eprintln!("[voxel] staging {} into {sw}:{remote_img} ...", image.display());
    if !scp_to(&ip, local, &remote_img) {
        clear_cached_ip(&sw);
        return Err(anyhow!("scp of the phase-2 image into {sw} failed"));
    }

    // Discover MGS's in-zone dropshot address (or take the override), then POST.
    // MGS binds the switch-zone underlay (an `fd..` ULA); fall back to any
    // :MGS_PORT LISTEN socket if the ULA heuristic misses.
    let mgs_expr = match mgs_override {
        Some(u) => format!("URL=\"{u}/recovery/host-phase2\""),
        None => format!(
            r#"ADDR=$(netstat -an -f inet6 2>/dev/null | awk '/\.{MGS_PORT} /&&/LISTEN/{{print $1}}' | grep '^fd' | head -1)
[ -z "$ADDR" ] && ADDR=$(netstat -an -f inet6 2>/dev/null | awk '/\.{MGS_PORT} /&&/LISTEN/{{print $1}}' | head -1)
[ -z "$ADDR" ] && {{ echo "[voxel] MGS :{MGS_PORT} not listening in {sw}"; exit 2; }}
HOST=$(printf '%s' "$ADDR" | sed 's/\.{MGS_PORT}$//')
URL="http://[$HOST]:{MGS_PORT}/recovery/host-phase2""#
        ),
    };
    let script = format!(
        r#"set -u
{mgs_expr}
echo "[voxel] POST $URL"
curl -s -X POST -H 'Content-Type: application/octet-stream' -H 'api-version: {GATEWAY_API_VERSION}' --data-binary @/var/tmp/voxel-phase2.img "$URL"
"#
    );
    let local_sh = std::env::temp_dir().join("voxel-mupdate-stage.sh");
    std::fs::write(&local_sh, &script).map_err(|e| anyhow!("write stage script: {e}"))?;
    let remote_sh = format!("{SWITCH_ZONE_ROOT}/var/tmp/voxel-mupdate-stage.sh");
    if !scp_to(&ip, local_sh.to_str().unwrap_or_default(), &remote_sh) {
        clear_cached_ip(&sw);
        return Err(anyhow!("scp of the stage script into {sw} failed"));
    }

    let out = ssh_capture(&ip, &zlogin("bash /var/tmp/voxel-mupdate-stage.sh"))
        .ok_or_else(|| anyhow!("ssh to {sw} for the MGS upload failed"))?;
    // Response is HostPhase2RecoveryImageId { sha256_hash } -> {"sha256_hash":"<64hex>"}.
    let hash = out
        .split(|c: char| !c.is_ascii_hexdigit())
        .find(|tok| tok.len() == 64)
        .ok_or_else(|| anyhow!("no sha256 in MGS response: {}", out.trim()))?;
    println!("staged host phase-2 in {sw} MGS");
    println!("  sha256: {hash}");
    println!("  pull:   voxel sp ipcc <sp> --cmd get-phase2:{hash}");
    Ok(())
}
