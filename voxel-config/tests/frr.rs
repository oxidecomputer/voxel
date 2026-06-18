//! Golden-file tests for generated `frr.conf`. Regenerate with
//! `EXPECTORATE=overwrite`.
//!
//! Models the a4x2 datacenter router roles under unnumbered BGP:
//! - a customer router (`cr1`) peers with both rack switches + the customer
//!   edge, and just relays (originates nothing - the rack originates the
//!   service pool, the edge originates the default).
//! - the customer edge (`ce`) peers with both CRs and originates the default
//!   route toward the rack.

use voxel_config::{FrrNeighbor, FrrRouter};

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
    };
    expectorate::assert_contents("tests/output/ce-frr.conf", &ce.render());
}
