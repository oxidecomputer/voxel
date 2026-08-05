//! Golden-file tests for the generated `config-rss.toml`.
//!
//! The fixtures under `tests/output/rss/` were serialized by omicron's own
//! `RackInitializeRequest` types, against omicron `cc07512e0`. They are the
//! contract for `VoxelConfig::to_config_rss`.
//!
//! Do not regenerate these with `EXPECTORATE=overwrite` to make a failing test
//! pass; that launders a serialization change into the baseline. Rebuild them
//! from omicron's types instead.

use voxel_config::VoxelConfig;

/// The scenarios span every axis that changes config-rss output: routing mode,
/// BFD, per-rack projection, and sled count.
fn config(scenario: &str) -> VoxelConfig {
    let network = match scenario {
        "bgp" | "multirack" | "a6x2" => {
            r#"router_mode = "bgp"
transit_bfd = false"#
        }
        "static" => {
            r#"router_mode = "static"
transit_bfd = false"#
        }
        "static-bfd" => {
            r#"router_mode = "static"
transit_bfd = true"#
        }
        other => panic!("unknown scenario {other}"),
    };
    // Multirack drops ce_external_ip (it is a single-rack host-route knob) and
    // a6x2 widens the rack; both are otherwise the default lab topology.
    let topology = match scenario {
        "multirack" => {
            r#"racks = 2
sleds = 3"#
        }
        "a6x2" => {
            r#"racks = 1
sleds = 6"#
        }
        _ => {
            r#"racks = 1
sleds = 3
ce_external_ip = "192.168.68.170""#
        }
    };
    let text = format!(
        r#"[topology]
{topology}
sled_memory_gb = 7

[network]
{network}
"#
    );
    VoxelConfig::from_toml(&text).expect("parse scenario config")
}

fn check(scenario: &str, rack: usize) {
    let rendered = config(scenario)
        .to_config_rss(rack)
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
