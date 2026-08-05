//! Golden-file tests for the generated `config-rss.toml`.
//!
//! The fixtures under `tests/output/rss/` were serialized by omicron's own
//! `RackInitializeRequest` types, against omicron `cc07512e0`. They are the
//! contract for `VoxelConfig::to_config_rss`.
//!
//! Do not regenerate these with `EXPECTORATE=overwrite` to make a failing test
//! pass; that launders a serialization change into the baseline. Rebuild them
//! from omicron's types instead.

use voxel_config::{ServicePoolSchema, VoxelConfig};

/// The scenarios span every axis that changes config-rss output: routing mode,
/// BFD, per-rack projection, and sled count.
fn config(scenario: &str) -> VoxelConfig {
    let network = match scenario {
        "bgp" | "multirack" | "a6x2" => "router_mode = \"bgp\"\ntransit_bfd = false",
        "static" => "router_mode = \"static\"\ntransit_bfd = false",
        "static-bfd" => "router_mode = \"static\"\ntransit_bfd = true",
        other => panic!("unknown scenario {other}"),
    };
    // Multirack drops ce_external_ip (it is a single-rack host-route knob) and
    // a6x2 widens the rack; both are otherwise the default lab topology.
    let topology = match scenario {
        "multirack" => "racks = 2\nsleds = 3",
        "a6x2" => "racks = 1\nsleds = 6",
        _ => "racks = 1\nsleds = 3\nce_external_ip = \"192.168.68.170\"",
    };
    let text = format!("[topology]\n{topology}\nsled_memory_gb = 7\n\n[network]\n{network}\n");
    VoxelConfig::from_toml(&text).expect("parse scenario config")
}

fn check(scenario: &str, rack: usize) {
    let rendered = config(scenario)
        .to_config_rss(rack, ServicePoolSchema::Ranges)
        .expect("render config-rss");
    expectorate::assert_contents(
        format!("tests/output/rss/{scenario}-rack{rack}.toml"),
        &rendered,
    );
}

#[test]
fn bgp_matches_rss_gen() {
    check("bgp", 0);
}

#[test]
fn static_matches_rss_gen() {
    check("static", 0);
}

#[test]
fn static_bfd_matches_rss_gen() {
    check("static-bfd", 0);
}

#[test]
fn a6x2_matches_rss_gen() {
    check("a6x2", 0);
}

/// Each rack is an independent RSS domain: rack 1's bootstrap set, trust-quorum
/// peers and customer/service network are all distinct from rack 0's.
#[test]
fn multirack_rack0_matches_rss_gen() {
    check("multirack", 0);
}

#[test]
fn multirack_rack1_matches_rss_gen() {
    check("multirack", 1);
}

/// omicron #10956 replaced the bare range list with a named pool. The two
/// shapes share one slot, so exactly one must ever appear.
#[test]
fn pools_schema_matches_omicron_main() {
    let rendered = config("bgp")
        .to_config_rss(0, ServicePoolSchema::Pools)
        .expect("render config-rss");
    expectorate::assert_contents("tests/output/rss/bgp-rack0-pools.toml", &rendered);
}

#[test]
fn exactly_one_service_pool_shape_is_emitted() {
    for (pools, want, unwanted) in [
        (
            ServicePoolSchema::Ranges,
            "internal_services_ip_pool_ranges",
            "service_ip_pools",
        ),
        (
            ServicePoolSchema::Pools,
            "service_ip_pools",
            "internal_services_ip_pool_ranges",
        ),
    ] {
        let keys = config("bgp").config_rss_keys(pools).expect("keys");
        assert!(keys.iter().any(|k| k == want), "{pools:?} missing {want}");
        assert!(
            !keys.iter().any(|k| k == unwanted),
            "{pools:?} still emits {unwanted}"
        );
    }
}
