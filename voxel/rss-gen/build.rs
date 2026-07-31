//! Per-omicron-era build detection for voxel-rss-gen.
//!
//! `build-rss-gen.sh` greps the pinned omicron for era-defining types and sets
//! an env var per finding. We surface each as a cfg so one source builds
//! correctly against any era:
//!
//! - `has_uplink_ports`: omicron#10651 wraps the sled-agent early-networking
//!   uplink port list in a non-empty `UplinkPorts` newtype, where v20-era
//!   omicron uses a bare `Vec<PortConfig>`.
//! - `has_service_ip_pools`: omicron#10941 replaces the bare
//!   `internal_services_ip_pool_ranges` range list with named
//!   `service_ip_pools` (ServiceIpPoolConfig) at RSS.

fn main() {
    println!("cargo:rustc-check-cfg=cfg(has_uplink_ports)");
    println!("cargo:rerun-if-env-changed=VOXEL_HAS_UPLINK_PORTS");
    if std::env::var_os("VOXEL_HAS_UPLINK_PORTS").is_some() {
        println!("cargo:rustc-cfg=has_uplink_ports");
    }
    println!("cargo:rustc-check-cfg=cfg(has_service_ip_pools)");
    println!("cargo:rerun-if-env-changed=VOXEL_HAS_SERVICE_IP_POOLS");
    if std::env::var_os("VOXEL_HAS_SERVICE_IP_POOLS").is_some() {
        println!("cargo:rustc-cfg=has_service_ip_pools");
    }
}
