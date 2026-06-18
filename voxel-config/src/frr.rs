//! FRR `frr.conf` generation for Voxel routers (unnumbered BGP).
//!
//! Grounded in maghemite `falcon-lab`'s working unnumbered setup: `neighbor
//! <iface> interface remote-as external`, `no bgp ebgp-requires-policy`, and
//! both address-families with `network` + `activate`. IPv4 prefixes are carried
//! over the IPv6 link-local session via RFC 5549 - FRR enables extended-nexthop
//! automatically for interface peers, so no explicit capability line is needed.

use std::fmt::Write as _;

/// An unnumbered eBGP neighbor reachable over an interface (no peer IP).
#[derive(Debug, Clone)]
pub struct FrrNeighbor {
    pub interface: String,
    pub description: String,
}

impl FrrNeighbor {
    pub fn new(interface: impl Into<String>, description: impl Into<String>) -> Self {
        Self { interface: interface.into(), description: description.into() }
    }
}

/// A router's FRR config: unnumbered eBGP on every listed interface, optionally
/// originating IPv4/IPv6 prefixes (`network` statements).
#[derive(Debug, Clone)]
pub struct FrrRouter {
    pub hostname: String,
    pub asn: u32,
    pub neighbors: Vec<FrrNeighbor>,
    pub originate4: Vec<String>,
    pub originate6: Vec<String>,
}

impl FrrRouter {
    /// Render a complete `frr.conf`.
    pub fn render(&self) -> String {
        let mut o = String::new();
        writeln!(o, "frr defaults datacenter").unwrap();
        writeln!(o, "hostname {}", self.hostname).unwrap();
        writeln!(o, "!").unwrap();

        for n in &self.neighbors {
            writeln!(o, "interface {}", n.interface).unwrap();
            writeln!(o, " description {}", n.description).unwrap();
            writeln!(o, "!").unwrap();
        }

        writeln!(o, "router bgp {}", self.asn).unwrap();
        writeln!(o, " no bgp ebgp-requires-policy").unwrap();
        for n in &self.neighbors {
            writeln!(o, " neighbor {} interface remote-as external", n.interface).unwrap();
            writeln!(o, " neighbor {} timers connect 1", n.interface).unwrap();
        }

        writeln!(o, " !").unwrap();
        self.render_afi(&mut o, "ipv4", &self.originate4);
        writeln!(o, " !").unwrap();
        self.render_afi(&mut o, "ipv6", &self.originate6);
        writeln!(o, "!").unwrap();
        o
    }

    fn render_afi(&self, o: &mut String, afi: &str, originate: &[String]) {
        writeln!(o, " address-family {afi} unicast").unwrap();
        for p in originate {
            writeln!(o, "  network {p}").unwrap();
        }
        for n in &self.neighbors {
            writeln!(o, "  neighbor {} activate", n.interface).unwrap();
        }
        writeln!(o, " exit-address-family").unwrap();
    }
}
