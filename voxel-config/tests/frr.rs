//! Golden-file tests for generated `frr.conf`. Regenerate with
//! `EXPECTORATE=overwrite`.
//!
//! Models the a4x2 datacenter router roles under unnumbered BGP:
//! - a customer router (`cr1`) peers with both rack switches + the customer
//!   edge, and just relays (originates nothing - the rack originates the
//!   service pool, the edge originates the default).
//! - the customer edge (`ce`) peers with both CRs and originates the default
//!   route toward the rack.

use voxel_config::{FrrNeighbor, FrrRouter, StaticUplink};

#[test]
fn cr1_unnumbered_relay() {
    let cr1 = FrrRouter {
        hostname: "cr1".into(),
        asn: 65101,
        neighbors: vec![
            FrrNeighbor::new("enp0s9", "to switch0"),
            FrrNeighbor::new("enp0s10", "to switch1"),
            FrrNeighbor::new("enp0s8", "to ce"),
        ],
        originate4: vec![],
        originate6: vec![],
        static_uplinks: vec![],
        track_bfd: false,
    };
    expectorate::assert_contents("tests/output/cr1-frr.conf", &cr1.render());
}

#[test]
fn ce_originates_default() {
    let ce = FrrRouter {
        hostname: "ce".into(),
        asn: 65100,
        neighbors: vec![
            FrrNeighbor::new("enp0s8", "to cr1"),
            FrrNeighbor::new("enp0s9", "to cr2"),
        ],
        originate4: vec!["0.0.0.0/0".into()],
        originate6: vec!["::/0".into()],
        static_uplinks: vec![],
        track_bfd: false,
    };
    expectorate::assert_contents("tests/output/ce-frr.conf", &ce.render());
}

fn cr1_static(track_bfd: bool) -> FrrRouter {
    // Static mode: numbered /30 toward the sidecar with a static route to the
    // rack pool, redistributed into the (unnumbered) eBGP session to ce.
    FrrRouter {
        hostname: "cr1".into(),
        asn: 65101,
        neighbors: vec![FrrNeighbor::new("enp0s8", "to ce")],
        originate4: vec![],
        originate6: vec![],
        static_uplinks: vec![StaticUplink {
            interface: "enp0s9".into(),
            address: "198.51.101.1/30".into(),
            peer: "198.51.101.2".into(),
            peer_asn: 65000,
            route: "198.51.100.0/24".into(),
        }],
        track_bfd,
    }
}

#[test]
fn cr1_static_plain() {
    // Default: plain static routes, no BFD (the a4x2 working config).
    expectorate::assert_contents("tests/output/cr1-static-frr.conf", &cr1_static(false).render());
}

#[test]
fn cr1_static_bfd() {
    // transit_bfd on: BFD-tracked routes + peers.
    expectorate::assert_contents("tests/output/cr1-static-bfd-frr.conf", &cr1_static(true).render());
}
