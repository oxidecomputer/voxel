//! Per-omicron-era build detection for voxel-rss-gen.
//!
//! Newer omicron wraps the sled-agent early-networking uplink port list in a
//! non-empty `UplinkPorts` newtype; v20-era omicron uses a bare `Vec<PortConfig>`.
//! `build-rss-gen.sh` greps the pinned omicron for the type and sets
//! `VOXEL_HAS_UPLINK_PORTS`; we surface it as the `has_uplink_ports` cfg so one
//! source builds correctly against any era.

fn main() {
    println!("cargo:rustc-check-cfg=cfg(has_uplink_ports)");
    println!("cargo:rerun-if-env-changed=VOXEL_HAS_UPLINK_PORTS");
    if std::env::var_os("VOXEL_HAS_UPLINK_PORTS").is_some() {
        println!("cargo:rustc-cfg=has_uplink_ports");
    }
}
